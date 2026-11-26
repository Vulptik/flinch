// Notification routing — all notifications go through the native overlay system.
// Falls back to bounded, serialized OS-toast delivery when native presentation fails.

mod fallback;

use crate::app::context::AppHandle;
use std::path::PathBuf;

use crate::overlay::notification::{
  ClickAction, NotificationManager, NotificationRequest, ThumbnailImage,
};
use crate::video::controller::RecordingStatus;

fn build_text_request(title: &str, body: &str, play_sound: bool) -> NotificationRequest {
  NotificationRequest {
    title: title.to_string(),
    body: body.to_string(),
    thumbnail: None,
    click_action: None,
    play_sound,
    critical: false,
    completed: None,
    processing: None,
  }
}

fn build_rich_request(
  title: &str,
  body: &str,
  thumbnail: Option<ThumbnailImage>,
  click_action: Option<ClickAction>,
  play_sound: bool,
) -> NotificationRequest {
  NotificationRequest {
    title: title.to_string(),
    body: body.to_string(),
    thumbnail,
    click_action,
    play_sound,
    critical: false,
    completed: None,
    processing: None,
  }
}

fn build_error_request(title: &str, body: &str, play_sound: bool) -> NotificationRequest {
  let mut request = build_text_request(title, body, play_sound);
  request.critical = true;
  request
}

fn should_silence_for_recording_status(status: RecordingStatus) -> bool {
  matches!(
    status,
    RecordingStatus::Recording | RecordingStatus::Paused | RecordingStatus::Error
  )
}

fn effective_play_sound_for_status(
  status: Option<RecordingStatus>,
  requested_play_sound: bool,
) -> bool {
  requested_play_sound && !status.is_some_and(should_silence_for_recording_status)
}

fn effective_play_sound(app: &AppHandle, requested_play_sound: bool) -> bool {
  let status = if let Some(recorder_state) = app.try_state::<crate::commands::VideoRecorderState>()
  {
    match recorder_state.0.try_lock() {
      Ok(mut recorder) => Some(recorder.status()),
      // Saving owns the recorder for potentially minutes. Notification routing
      // must never wait on that lock; remain quiet if its state cannot be read.
      Err(std::sync::TryLockError::WouldBlock) => return false,
      Err(error) => {
        tracing::warn!(%error, "Notification recorder status unavailable");
        return false;
      },
    }
  } else {
    None
  };
  effective_play_sound_for_status(status, requested_play_sound)
}

fn should_suppress_notification(app: &AppHandle) -> bool {
  crate::commands::current_recording_session(app)
    .is_some_and(|session| session.presentation_mode.is_silent())
}

pub fn should_display_notification(app: &AppHandle, notifications_enabled: bool) -> bool {
  notifications_enabled && !should_suppress_notification(app)
}

/// Show a text-only notification via the overlay system.
///
/// This is the backward-compatible entry point. All existing callsites
/// use this signature: `show(app, title, body)`.
pub fn show(app: &AppHandle, title: &str, body: &str) {
  if !should_display_notification(app, true) {
    return;
  }
  let request = build_text_request(title, body, effective_play_sound(app, true));

  if let Some(manager) = app.try_state::<NotificationManager>() {
    manager.notify(request, Some(app));
  } else {
    // Manager not registered yet (startup race) — fire OS toast as fallback
    show_os_toast(app, title, body);
  }
}

/// Error titles identify the failure; catalog bodies explain the consequence and point to logs.
pub fn show_error(app: &AppHandle, title: &str, body_key: &str) {
  match crate::i18n::native_settings(&crate::i18n::get_lang(app), body_key) {
    Ok(body) => {
      let request = build_error_request(title, &body, effective_play_sound(app, true));
      if let Some(manager) = app.try_state::<NotificationManager>() {
        manager.notify(request, Some(app));
      } else {
        show_os_toast(app, title, &body);
      }
    },
    Err(error) => tracing::error!(%error, body_key, "Error notification localization failed"),
  }
}

pub struct ProcessingNotice(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for ProcessingNotice {
  fn drop(&mut self) {
    self.0.store(true, std::sync::atomic::Ordering::Release);
  }
}

/// Persistent feedback; completed work never leaves a stale queued status.
pub fn recording_processing(app: &AppHandle, gif: bool) -> Option<ProcessingNotice> {
  recording_processing_with_progress(
    app,
    gif,
    std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
  )
}

pub fn recording_processing_with_progress(
  app: &AppHandle,
  gif: bool,
  stage: std::sync::Arc<std::sync::atomic::AtomicU8>,
) -> Option<ProcessingNotice> {
  let config = crate::config::Config::current(app);
  if !should_display_notification(app, config.notifications_enabled) {
    return None;
  }
  let key = if gif {
    "recording_processing_gif"
  } else {
    "recording_processing_video"
  };
  let title = match crate::i18n::native_settings(&crate::i18n::get_lang(app), key) {
    Ok(title) => title,
    Err(error) => {
      tracing::error!(%error, "Processing notification localization failed");
      return None;
    },
  };
  let keys: &[&str] = if gif {
    &["recording_processing_wait"]
  } else {
    &[
      "recording_processing_closing",
      "recording_processing_preparing",
      "recording_processing_joining",
      "recording_processing_audio",
      "recording_processing_checking",
      "recording_processing_saving",
    ]
  };
  let labels = match keys
    .iter()
    .map(|key| crate::i18n::native_settings(&crate::i18n::get_lang(app), key))
    .collect::<Result<Vec<_>, _>>()
  {
    Ok(labels) => labels,
    Err(error) => {
      tracing::error!(%error, "Processing stage localization failed");
      return None;
    },
  };
  let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
  let mut request = build_text_request(&title, "", false);
  request.completed = Some(completed.clone());
  request.processing = Some(crate::overlay::notification::ProcessingFeedback {
    started: std::time::Instant::now(),
    stage,
    labels,
  });
  if let Some(manager) = app.try_state::<NotificationManager>() {
    manager.notify(request, Some(app));
  } else {
    tracing::warn!("Notification manager unavailable for recording processing feedback");
    return None;
  }
  Some(ProcessingNotice(completed))
}

/// Show a rich notification with optional thumbnail and click action.
pub fn show_rich(
  app: &AppHandle,
  title: &str,
  body: &str,
  thumbnail: Option<ThumbnailImage>,
  click_action: Option<ClickAction>,
) {
  if !should_display_notification(app, true) {
    return;
  }
  let request = build_rich_request(
    title,
    body,
    thumbnail,
    click_action,
    effective_play_sound(app, true),
  );

  if let Some(manager) = app.try_state::<NotificationManager>() {
    manager.notify(request, Some(app));
  } else {
    show_os_toast(app, title, body);
  }
}

/// Show a rich notification that opens a folder (selecting the file) when clicked.
pub fn show_with_open_folder(app: &AppHandle, title: &str, body: &str, path: PathBuf) {
  show_rich(app, title, body, None, Some(ClickAction::OpenFolder(path)));
}

/// Thumbnails are cropped into the notification's slot, so this leaves room at any scale.
const SAVED_FILE_THUMBNAIL: u32 = 256;
/// How long a file's notification waits for its thumbnail before going without.
const SAVED_FILE_THUMBNAIL_WAIT: std::time::Duration = std::time::Duration::from_millis(400);

/// A notification about a capture file (a saved recording or GIF, a recent capture copied again),
/// with the recording engine's picture of its last frame when the file was just recorded, else
/// the thumbnail Explorer shows for it when the Shell provides one quickly; otherwise it appears
/// without one. Clicking it shows the file in its folder.
pub async fn show_file(app: &AppHandle, title: &str, body: &str, path: PathBuf) {
  if !should_display_notification(app, true) {
    return;
  }
  // The notification manager applies the user's preference; skip only the extraction it would
  // throw away.
  let remembered = crate::thumbnails::take_remembered(&path);
  let thumbnail = if !crate::config::Config::current(app).notifications_enabled {
    None
  } else if let Some(pixels) = remembered {
    // The recording engine's picture of the file's last frame: opaque, so already straight.
    Some(ThumbnailImage {
      rgba: pixels.rgba,
      width: pixels.width,
      height: pixels.height,
    })
  } else {
    match crate::thumbnails::extract_bounded(
      path.clone(),
      SAVED_FILE_THUMBNAIL,
      SAVED_FILE_THUMBNAIL_WAIT,
    )
    .await
    {
      Ok(pixels) => {
        let pixels = pixels.into_straight();
        Some(ThumbnailImage {
          rgba: pixels.rgba,
          width: pixels.width,
          height: pixels.height,
        })
      },
      Err(error) => {
        tracing::info!(%error, "File notification shows no thumbnail");
        None
      },
    }
  };
  show_rich(
    app,
    title,
    body,
    thumbnail,
    Some(ClickAction::OpenFolder(path)),
  );
}

/// Direct Windows toast. Used as crash-recovery fallback
/// when the overlay thread has permanently died (after max respawn attempts).
pub fn show_os_toast(_app: &AppHandle, title: &str, body: &str) {
  show_os_toast_text(title, body);
}

pub(crate) fn show_os_toast_text(title: &str, body: &str) {
  fallback::enqueue(title, body);
}

#[cfg(test)]
mod tests {
  #[test]
  fn saved_files_notify_with_their_shell_thumbnail_unless_notifications_are_off() {
    use crate::overlay::notification::{ClickAction, NotificationManager};
    let folder = tempfile::tempdir().unwrap();
    let saved = folder.path().join("recording.png");
    image::RgbaImage::from_pixel(64, 36, image::Rgba([217, 47, 47, 255]))
      .save(&saved)
      .unwrap();
    let runtime = crate::app::runtime::test_runtime();
    for enabled in [true, false] {
      let app = crate::app::context::AppHandle::isolated(
        crate::config::Config {
          language: "en".into(),
          notifications_enabled: enabled,
          notification_sound: false,
          ..Default::default()
        },
        std::path::PathBuf::new(),
      );
      runtime.block_on(super::show_file(&app, "Saved", "body", saved.clone()));
      let requests = app
        .try_state::<NotificationManager>()
        .unwrap()
        .take_test_requests();
      assert_eq!(requests.len(), 1, "notifications enabled: {enabled}");
      assert!(
        matches!(&requests[0].click_action, Some(ClickAction::OpenFolder(path)) if path == &saved)
      );
      match (&requests[0].thumbnail, enabled) {
        (Some(thumbnail), true) => {
          assert!(thumbnail.width > 0 && thumbnail.height > 0);
          assert_eq!(&thumbnail.rgba[..4], &[217, 47, 47, 255]);
        },
        (None, false) => {},
        (thumbnail, _) => panic!("enabled {enabled}, thumbnail {}", thumbnail.is_some()),
      }
    }
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        notifications_enabled: true,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let gone = folder.path().join("gone.mp4");
    runtime.block_on(super::show_file(&app, "Saved", "body", gone));
    let requests = app
      .try_state::<NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(
      requests.len(),
      1,
      "a missing thumbnail never drops the notification"
    );
    assert!(requests[0].thumbnail.is_none());
  }

  #[test]
  fn a_just_recorded_file_shows_the_engines_picture_without_asking_the_shell() {
    // Regression: a 90 s 1440p60 recording's notification had no thumbnail because the Shell
    // took longer than the notification waits. The engine's own picture needs no Shell at all:
    // this file does not even exist, so a Shell extraction could only fail.
    use crate::overlay::notification::NotificationManager;
    let folder = tempfile::tempdir().unwrap();
    let saved = folder.path().join("never-written.mp4");
    let pixels = crate::thumbnails::Pixels {
      width: 2,
      height: 1,
      rgba: vec![10, 20, 30, 255, 40, 50, 60, 255],
    };
    crate::thumbnails::remember(&saved, pixels);
    // Another file's notification leaves the picture for its own file.
    let other = folder.path().join("other.mp4");
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config {
        language: "en".into(),
        notifications_enabled: true,
        notification_sound: false,
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    let runtime = crate::app::runtime::test_runtime();
    runtime.block_on(super::show_file(&app, "Saved", "body", other));
    runtime.block_on(super::show_file(&app, "Saved", "body", saved.clone()));
    let requests = app
      .try_state::<NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].thumbnail.is_none());
    let thumbnail = requests[1].thumbnail.as_ref().expect("engine picture");
    assert_eq!((thumbnail.width, thumbnail.height), (2, 1));
    assert_eq!(thumbnail.rgba, [10, 20, 30, 255, 40, 50, 60, 255]);
    // Handed out once.
    assert!(crate::thumbnails::take_remembered(&saved).is_none());
  }

  #[test]
  fn critical_errors_bypass_preferences_but_obsolete_status_does_not() {
    let error = super::build_error_request("Save failed", "Not saved", false);
    let ordinary = super::build_text_request("Saved", "capture.png", false);
    for enabled in [false, true] {
      assert!(error.should_display(enabled));
      assert_eq!(ordinary.should_display(enabled), enabled);
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mut obsolete = error;
    obsolete.completed = Some(done);
    assert!(!obsolete.should_display(false));
    assert!(!obsolete.should_display(true));
  }

  #[test]
  fn completed_processing_notices_are_obsolete_before_saved_feedback() {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut request = super::build_text_request("Processing", "", false);
    request.completed = Some(done.clone());
    let notice = super::ProcessingNotice(done);
    assert!(!request.is_obsolete());
    assert!(!request.play_sound);
    drop(notice);
    assert!(request.is_obsolete());
  }
  use super::*;

  #[test]
  fn notification_sound_never_waits_for_a_busy_finalizer() {
    let folder = tempfile::tempdir().unwrap();
    let app = crate::app::context::AppHandle::isolated(
      crate::config::Config::default(),
      folder.path().to_path_buf(),
    );
    let state = app.state::<crate::commands::VideoRecorderState>();
    let busy = state.0.lock().unwrap();
    let worker_app = app.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
      tx.send(super::effective_play_sound(&worker_app, true))
        .unwrap()
    });
    let response = rx.recv_timeout(std::time::Duration::from_secs(1));
    drop(busy);
    worker.join().unwrap();
    assert!(!response.unwrap());
  }

  #[test]
  fn build_text_request_sets_expected_fields() {
    let request = build_text_request("Title", "Body", true);

    assert_eq!(request.title, "Title");
    assert_eq!(request.body, "Body");
    assert!(request.thumbnail.is_none());
    assert!(request.click_action.is_none());
    assert!(request.play_sound);
  }

  #[test]
  fn build_text_request_can_disable_sound() {
    let request = build_text_request("Title", "Body", false);

    assert!(!request.play_sound);
  }

  #[test]
  fn build_rich_request_preserves_thumbnail_and_click_action() {
    let thumbnail = ThumbnailImage {
      rgba: vec![0, 1, 2, 3],
      width: 1,
      height: 1,
    };
    let request = build_rich_request(
      "Saved",
      "Opened on click",
      Some(thumbnail.clone()),
      Some(ClickAction::OpenFolder(PathBuf::from(
        r"C:\temp\capture.png",
      ))),
      true,
    );

    assert_eq!(request.title, "Saved");
    assert_eq!(request.body, "Opened on click");
    assert!(request.play_sound);

    let preserved_thumbnail = request.thumbnail.expect("thumbnail should be present");
    assert_eq!(preserved_thumbnail.rgba, thumbnail.rgba);
    assert_eq!(preserved_thumbnail.width, thumbnail.width);
    assert_eq!(preserved_thumbnail.height, thumbnail.height);

    match request.click_action {
      Some(ClickAction::OpenFolder(path)) => {
        assert_eq!(path, PathBuf::from(r"C:\temp\capture.png"));
      },
      _ => panic!("expected open-folder click action"),
    }
  }

  #[test]
  fn processing_text_rich_and_error_notices_reach_the_manager_with_their_stages() {
    use crate::overlay::notification::{ClickAction, NotificationManager};
    use std::sync::{
      Arc,
      atomic::{AtomicU8, Ordering},
    };
    let isolated = |enabled| {
      crate::app::context::AppHandle::isolated(
        crate::config::Config {
          language: "en".into(),
          notifications_enabled: enabled,
          ..Default::default()
        },
        std::path::PathBuf::new(),
      )
    };
    let text = |key| crate::i18n::native_settings("en", key).unwrap();
    let app = isolated(true);
    let manager = app.try_state::<NotificationManager>().unwrap();

    let stage = Arc::new(AtomicU8::new(3));
    let video = super::recording_processing_with_progress(&app, false, stage.clone())
      .expect("video processing notice");
    let gif = super::recording_processing(&app, true).expect("GIF processing notice");
    let requests = manager.take_test_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].title, text("recording_processing_video"));
    assert_eq!(requests[1].title, text("recording_processing_gif"));
    for request in &requests {
      assert!(request.body.is_empty() && !request.play_sound && !request.critical);
      assert!(!request.is_obsolete(), "work in progress keeps its notice");
    }
    let video_stages = requests[0].processing.as_ref().unwrap();
    assert_eq!(
      video_stages.labels,
      [
        "recording_processing_closing",
        "recording_processing_preparing",
        "recording_processing_joining",
        "recording_processing_audio",
        "recording_processing_checking",
        "recording_processing_saving",
      ]
      .map(text)
    );
    assert!(Arc::ptr_eq(&video_stages.stage, &stage));
    stage.store(4, Ordering::Release);
    assert_eq!(video_stages.stage.load(Ordering::Acquire), 4);
    assert_eq!(
      requests[1].processing.as_ref().unwrap().labels,
      [text("recording_processing_wait")]
    );
    // Finishing the work makes each queued notice obsolete, so none lingers.
    drop(video);
    assert!(requests[0].is_obsolete());
    assert!(!requests[1].is_obsolete());
    drop(gif);
    assert!(requests[1].is_obsolete());

    // Routine processing feedback follows the preference; text and rich notices are filtered
    // later by the manager, and errors stay critical.
    let quiet = isolated(false);
    assert!(super::recording_processing(&quiet, false).is_none());
    assert!(
      quiet
        .try_state::<NotificationManager>()
        .unwrap()
        .take_test_requests()
        .is_empty()
    );
    super::show(&app, "Title", "Body");
    super::show_rich(
      &app,
      "Rich",
      "Body",
      None,
      Some(ClickAction::OpenFolder(PathBuf::from(r"C:\temp\a.png"))),
    );
    super::show_error(&app, "Failed", "recent_error_copy");
    super::show_error(&app, "Failed", "not_a_catalog_key");
    let requests = manager.take_test_requests();
    assert_eq!(
      requests.len(),
      3,
      "an unknown error text is logged, never shown"
    );
    assert_eq!(
      (requests[0].title.as_str(), requests[0].body.as_str()),
      ("Title", "Body")
    );
    assert!(!requests[0].critical && requests[0].click_action.is_none());
    assert!(matches!(
      &requests[1].click_action,
      Some(ClickAction::OpenFolder(path)) if path == &PathBuf::from(r"C:\temp\a.png")
    ));
    assert!(requests[2].critical);
    assert_eq!(requests[2].body, text("recent_error_copy"));
  }

  #[test]
  fn show_with_open_folder_builds_expected_click_action() {
    let open_folder = build_rich_request(
      "Folder",
      "Body",
      None,
      Some(ClickAction::OpenFolder(PathBuf::from(
        r"C:\temp\capture.png",
      ))),
      true,
    );
    match open_folder.click_action {
      Some(ClickAction::OpenFolder(path)) => {
        assert_eq!(path, PathBuf::from(r"C:\temp\capture.png"));
      },
      _ => panic!("expected open-folder click action"),
    }
  }

  #[test]
  fn recording_statuses_that_should_be_silent_match_expected_states() {
    assert!(!should_silence_for_recording_status(RecordingStatus::Idle));
    assert!(!should_silence_for_recording_status(
      RecordingStatus::Starting
    ));
    assert!(should_silence_for_recording_status(
      RecordingStatus::Recording
    ));
    assert!(should_silence_for_recording_status(RecordingStatus::Paused));
    assert!(!should_silence_for_recording_status(
      RecordingStatus::Stopping
    ));
    assert!(should_silence_for_recording_status(RecordingStatus::Error));
  }

  #[test]
  fn effective_play_sound_respects_requested_sound_flag() {
    assert!(effective_play_sound_for_status(None, true));
    assert!(effective_play_sound_for_status(
      Some(RecordingStatus::Idle),
      true
    ));
    assert!(!effective_play_sound_for_status(
      Some(RecordingStatus::Recording),
      true
    ));
    assert!(!effective_play_sound_for_status(
      Some(RecordingStatus::Paused),
      true
    ));
    assert!(!effective_play_sound_for_status(
      Some(RecordingStatus::Error),
      true
    ));
    assert!(!effective_play_sound_for_status(
      Some(RecordingStatus::Idle),
      false
    ));
  }

  #[test]
  fn silent_recording_sessions_should_suppress_notifications() {
    let session = crate::state::ActiveRecordingSession {
      kind: crate::state::ActiveRecordingKind::Gif,
      paused: false,
      presentation_mode: crate::recording_visibility::RecordingPresentationMode::Silent,
    };
    assert!(session.presentation_mode.is_silent());
  }
}
