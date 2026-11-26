use super::*;

#[tokio::test]
async fn start_command_validates_mode_cancellation_and_failed_launch_without_capture() {
  for mode in ["area", "fullscreen", "active_window", "unknown"] {
    for scenario in 0..5 {
      let app = AppHandle::isolated(
        crate::config::Config {
          language: "en".into(),
          video_fps: 24,
          notifications_enabled: false,
          notification_sound: false,
          ..Default::default()
        },
        PathBuf::new(),
      );
      let result = start_video_recording_with(
        app.clone(),
        -100000,
        -100000,
        64,
        48,
        0,
        mode.into(),
        |_| match scenario {
          0 => Ok(None),
          1 => Err("visibility service unavailable".into()),
          2 => Ok(Some(RecordingPresentationMode::Silent)),
          _ => Ok(Some(RecordingPresentationMode::FullUi)),
        },
        |_, bounds, fps, parsed_mode, presentation| async move {
          assert_eq!(bounds, (-100000, -100000, 64, 48));
          assert_eq!(fps, 0);
          assert_eq!(parsed_mode, mode.parse::<RecordingMode>().unwrap());
          assert_eq!(
            presentation,
            if scenario == 2 {
              RecordingPresentationMode::Silent
            } else {
              RecordingPresentationMode::FullUi
            }
          );
          if scenario == 4 {
            Err("encoder startup failed".into())
          } else {
            Ok("fixture.mp4".into())
          }
        },
      )
      .await;
      if mode == "unknown" {
        assert!(result.unwrap_err().contains("Invalid recording mode"));
      } else {
        match scenario {
          0 => {
            let value = result.unwrap();
            assert!(matches!(
              value.status,
              crate::commands::video::RecordingLifecycleStatus::Idle
            ));
            assert!(value.path.is_none());
          },
          1 => assert_eq!(result.unwrap_err(), "visibility service unavailable"),
          4 => assert_eq!(result.unwrap_err(), "encoder startup failed"),
          _ => {
            let value = result.unwrap();
            assert!(matches!(
              value.status,
              crate::commands::video::RecordingLifecycleStatus::Recording
            ));
            assert_eq!(value.path.as_deref(), Some("fixture.mp4"));
            assert!(value.source_app.is_none());
          },
        }
      }
      assert!(crate::commands::current_recording_session(&app).is_none());
    }
  }
}

#[test]
fn orphan_cleanup_is_scoped_to_the_supplied_root_and_preserves_siblings_and_failed_reads() {
  let folder = tempfile::tempdir().unwrap();
  let root = folder.path().join("owned-recordings");
  assert_eq!(cleanup_recording_root(&root).unwrap(), 0);
  std::fs::create_dir(&root).unwrap();
  let sibling = folder.path().join("preserved.txt");
  std::fs::write(&sibling, b"preserved").unwrap();
  for name in ["one.mp4", "two.pcm"] {
    std::fs::write(root.join(name), b"interrupted").unwrap();
  }
  assert_eq!(cleanup_recording_root(&root).unwrap(), 2);
  assert!(!root.exists());
  assert_eq!(std::fs::read(&sibling).unwrap(), b"preserved");
  std::fs::create_dir(&root).unwrap();
  assert_eq!(cleanup_recording_root(&root).unwrap(), 1);
  assert!(
    cleanup_recording_root(&sibling)
      .unwrap_err()
      .to_string()
      .contains("Failed to read recording temp root")
  );
  assert_eq!(std::fs::read(&sibling).unwrap(), b"preserved");
}

#[test]
fn startup_cleanup_retains_interrupted_native_engine_sessions() {
  let folder = tempfile::tempdir().unwrap();
  let root = folder.path().join("recordings");
  // The engine stages beside the planned temporary output, one directory deeper, and writes
  // its own session.json rather than the recorder's recovery.json.
  let native = root.join("video-recording-crashed").join(format!(
    "{}1234-5678",
    flinch_video_engine::artifact::SESSION_DIRECTORY_PREFIX
  ));
  std::fs::create_dir_all(&native).unwrap();
  std::fs::write(
    native.join("capture.partial.mp4"),
    b"valuable captured data",
  )
  .unwrap();
  std::fs::write(native.join("session.json"), b"{}").unwrap();
  let finished = root.join("video-recording-finished");
  std::fs::create_dir_all(finished.join("unrelated")).unwrap();
  assert_eq!(cleanup_recording_root(&root).unwrap(), 1);
  assert!(native.join("capture.partial.mp4").exists());
  assert!(!finished.exists());
}

#[test]
fn startup_cleanup_retains_failed_and_interrupted_recordings() {
  let folder = tempfile::tempdir().unwrap();
  let root = folder.path().join("recordings");
  for name in ["failed", "interrupted"] {
    let session = root.join(name);
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("segment.mp4"), b"valuable captured data").unwrap();
  }
  std::fs::write(root.join("failed/recovery.json"), b"{}").unwrap();
  std::fs::create_dir(root.join("empty")).unwrap();
  assert_eq!(cleanup_recording_root(&root).unwrap(), 1);
  assert!(root.join("failed/segment.mp4").exists());
  assert!(root.join("interrupted/segment.mp4").exists());
  assert!(!root.join("empty").exists());
}

fn recovery_app() -> AppHandle {
  AppHandle::isolated(
    crate::config::Config {
      language: "en".into(),
      ..Default::default()
    },
    PathBuf::new(),
  )
}

#[test]
fn a_recovered_recording_is_announced_with_its_path_and_opens_its_folder() {
  use crate::overlay::notification::{ClickAction, NotificationManager};
  use crate::video::recovery::Recovered;
  let app = recovery_app();
  let saved = PathBuf::from(r"C:\Videos\clip (1).mp4");
  report_recovery(
    &app,
    std::path::Path::new("session"),
    Ok(Recovered::Saved(saved.clone())),
  );
  let sent = app
    .try_state::<NotificationManager>()
    .expect("isolated notifications")
    .take_test_requests();
  assert_eq!(sent.len(), 1);
  assert_eq!(
    sent[0].title,
    crate::i18n::native_settings("en", "recording_recovered").expect("title")
  );
  assert!(
    sent[0].body.ends_with(r"C:\Videos\clip (1).mp4"),
    "{}",
    sent[0].body
  );
  assert!(matches!(&sent[0].click_action, Some(ClickAction::OpenFolder(path)) if *path == saved));

  // Nothing to say for an empty or still-open session; a failure is reported as one error.
  report_recovery(&app, std::path::Path::new("session"), Ok(Recovered::Empty));
  report_recovery(&app, std::path::Path::new("session"), Ok(Recovered::InUse));
  assert!(
    app
      .try_state::<NotificationManager>()
      .expect("isolated notifications")
      .take_test_requests()
      .is_empty()
  );
  report_recovery(
    &app,
    std::path::Path::new("session"),
    Err("Recover: torn".into()),
  );
  let sent = app
    .try_state::<NotificationManager>()
    .expect("isolated notifications")
    .take_test_requests();
  assert_eq!(sent.len(), 1);
  assert_eq!(
    sent[0].title,
    crate::i18n::native_settings("en", "recording_recovery_failed").expect("title")
  );
  assert_eq!(
    sent[0].body,
    crate::i18n::native_settings("en", "notification_error_recovery").expect("body")
  );
  assert!(
    sent[0].critical,
    "a lost recording is never a routine notification"
  );
}

#[test]
fn launch_recovery_runs_off_the_caller_and_leaves_this_process_sessions_alone() {
  let app = recovery_app();
  let root = tempfile::tempdir().expect("root");
  let output = root
    .path()
    .join("video-clip-1-2")
    .join("clip.flinch-part.mp4");
  let destination = root.path().join("clip.mp4");
  crate::video::recovery::write_marker(&output, &destination, false).expect("marker");
  recover_in(&app, root.path().to_path_buf())
    .expect("spawn")
    .join()
    .expect("recovery thread");
  assert!(
    output.parent().expect("folder").is_dir(),
    "a live session is never touched"
  );
  assert!(
    app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .expect("isolated notifications")
      .take_test_requests()
      .is_empty()
  );
}

#[test]
fn a_recovered_gif_that_cannot_be_decoded_reports_the_encoder_error() {
  let root = tempfile::tempdir().expect("root");
  let input = root.path().join("recovered.mp4");
  std::fs::write(&input, b"not a recording").expect("input");
  let output = root.path().join("clip.gif");
  let error = encode_recovered_gif(&input, &output).expect_err("undecodable");
  assert!(
    error.starts_with("Encode") && error.contains("clip.gif"),
    "{error}"
  );
  assert!(!output.exists());
}
