use super::*;
use crate::recording_visibility::RecordingPresentationMode;
use crate::state::{
  ActiveRecordingKind, OverlayActivityState, RecordingSessionState, VideoRecorderState,
};
use std::time::Duration;

fn app() -> AppHandle {
  AppHandle::isolated(
    crate::config::Config {
      language: "en".into(),
      notifications_enabled: false,
      notification_sound: false,
      ..Default::default()
    },
    std::path::PathBuf::new(),
  )
}
fn poison(app: &AppHandle) {
  let recorder = app.state::<VideoRecorderState>().0.clone();
  assert!(
    std::thread::spawn(move || {
      let _guard = recorder.lock().unwrap();
      panic!("injected recorder failure");
    })
    .join()
    .is_err()
  );
}

#[tokio::test]
async fn selection_claim_is_released_on_cancel_failure_and_before_recorder_start() {
  use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  };
  for scenario in 0..6 {
    let app = app();
    let calls = Arc::new(AtomicUsize::new(0));
    let select_calls = calls.clone();
    let start_calls = calls.clone();
    let selected_app = app.clone();
    RecordingCoordinator::select_area_with(
      app.clone(),
      move |_| match scenario {
        0 => Ok(None),
        1 => Err("visibility lookup failed".into()),
        _ => Ok(Some(RecordingPresentationMode::Silent)),
      },
      move || async move {
        assert!(
          selected_app
            .try_state::<OverlayActivityState>()
            .unwrap()
            .is_video_area_selection_active()
        );
        select_calls.fetch_add(1, Ordering::SeqCst);
        match scenario {
          2 => Ok(None),
          3 => anyhow::bail!("selection owner disconnected"),
          _ => Ok(Some(crate::overlay::AreaRecordingResult {
            x: -40,
            y: -20,
            width: 64,
            height: 48,
            screen_width: 640,
            screen_height: 480,
            source_app: Some("fixture".into()),
          })),
        }
      },
      move |app, region, mode| async move {
        assert!(
          !app
            .try_state::<OverlayActivityState>()
            .unwrap()
            .is_any_active()
        );
        assert_eq!(
          (region.x, region.y, region.width, region.height),
          (-40, -20, 64, 48)
        );
        assert_eq!(region.source_app.as_deref(), Some("fixture"));
        assert_eq!(mode, RecordingPresentationMode::Silent);
        start_calls.fetch_add(10, Ordering::SeqCst);
        if scenario == 4 {
          Err("encoder unavailable".into())
        } else {
          Ok("fixture.mp4".into())
        }
      },
    )
    .await;
    assert_eq!(
      calls.load(Ordering::SeqCst),
      if scenario < 2 {
        0
      } else if scenario < 4 {
        1
      } else {
        11
      }
    );
    assert!(
      !app
        .try_state::<OverlayActivityState>()
        .unwrap()
        .is_any_active()
    );
    assert!(RecordingCoordinator::current_session(&app).is_none());
  }
}

#[tokio::test]
async fn dropped_selection_future_releases_its_claim_for_the_next_capture() {
  let app = app();
  let operation = RecordingCoordinator::select_area_with(
    app.clone(),
    |_| Ok(Some(RecordingPresentationMode::Silent)),
    std::future::pending,
    |_, _, _| async { panic!("selection did not complete") },
  );
  assert!(
    tokio::time::timeout(Duration::from_millis(5), operation)
      .await
      .is_err()
  );
  let state = app.try_state::<OverlayActivityState>().unwrap();
  assert!(
    !state.is_any_active(),
    "dropped selection must not block every future capture"
  );
  assert!(state.try_claim_video_area_selection());
  state.release_video_area_selection();
}

#[tokio::test]
async fn area_shortcut_preserves_each_existing_overlay_claim_without_opening_another() {
  for kind in 0..3 {
    let app = app();
    let state = app.try_state::<OverlayActivityState>().unwrap();
    assert!(match kind {
      0 => state.try_claim_screenshot(),
      1 => state.try_claim_gif(),
      _ => state.try_claim_video_area_selection(),
    });
    for _ in 0..3 {
      tokio::time::timeout(
        Duration::from_secs(1),
        RecordingCoordinator::toggle_area_shortcut(app.clone()),
      )
      .await
      .unwrap();
      assert!(state.is_any_active());
      assert_eq!(state.is_screenshot_active(), kind == 0);
      assert_eq!(state.is_gif_active(), kind == 1);
      assert_eq!(state.is_video_area_selection_active(), kind == 2);
      assert!(RecordingCoordinator::current_session(&app).is_none());
    }
  }
}

#[tokio::test]
async fn stale_idle_session_is_cleared_before_tray_pause_without_sending_controls() {
  for kind in [ActiveRecordingKind::Video, ActiveRecordingKind::Gif] {
    let app = app();
    app
      .try_state::<RecordingSessionState>()
      .unwrap()
      .set(kind, RecordingPresentationMode::Silent);
    RecordingCoordinator::toggle_tray_pause(app.clone()).await;
    assert!(
      app
        .try_state::<RecordingSessionState>()
        .unwrap()
        .snapshot()
        .is_none()
    );
    assert!(
      app
        .try_state::<crate::overlay::notification::NotificationManager>()
        .unwrap()
        .take_test_requests()
        .is_empty()
    );
  }
}

#[tokio::test]
async fn failed_stop_prevents_every_shortcut_from_starting_a_second_capture() {
  for trigger in 0..3 {
    let app = app();
    app.try_state::<RecordingSessionState>().unwrap().set(
      ActiveRecordingKind::Video,
      RecordingPresentationMode::Silent,
    );
    poison(&app);
    let operation = async {
      match trigger {
        0 => RecordingCoordinator::toggle_area_shortcut(app.clone()).await,
        1 => RecordingCoordinator::toggle_fullscreen_shortcut(app.clone()).await,
        _ => RecordingCoordinator::toggle_active_window_shortcut(app.clone()).await,
      }
    };
    tokio::time::timeout(Duration::from_secs(1), operation)
      .await
      .unwrap();
    assert!(
      !app
        .try_state::<OverlayActivityState>()
        .unwrap()
        .is_any_active()
    );
    assert!(
      app
        .try_state::<RecordingSessionState>()
        .unwrap()
        .snapshot()
        .is_some(),
      "a failed stop must not be mistaken for permission to start another recording"
    );
  }
}

#[tokio::test]
async fn failed_tray_pause_and_resume_clear_session_and_deliver_critical_failure() {
  for paused in [false, true] {
    let app = app();
    let state = app.try_state::<RecordingSessionState>().unwrap();
    state.set(
      ActiveRecordingKind::Video,
      RecordingPresentationMode::Silent,
    );
    state.set_paused(paused);
    poison(&app);
    tokio::time::timeout(
      Duration::from_secs(2),
      RecordingCoordinator::toggle_tray_pause(app.clone()),
    )
    .await
    .unwrap();
    assert!(state.snapshot().is_none());
    let notices = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].critical);
  }
}

#[tokio::test]
async fn a_failed_tray_stop_is_logged_and_never_starts_another_recording() {
  let app = app();
  let state = app.try_state::<RecordingSessionState>().unwrap();
  state.set(
    ActiveRecordingKind::Video,
    RecordingPresentationMode::Silent,
  );
  poison(&app);
  tokio::time::timeout(
    Duration::from_secs(2),
    RecordingCoordinator::toggle_tray_record(app.clone()),
  )
  .await
  .unwrap();
  assert!(
    !app
      .try_state::<OverlayActivityState>()
      .unwrap()
      .is_any_active(),
    "a failed stop opens no selection"
  );
  assert_eq!(
    state.snapshot().map(|session| session.kind),
    Some(ActiveRecordingKind::Video),
    "the recording stays listed so Stop can be retried"
  );
  // Regression: this failure used to reach only the log, so Stop looked like it did nothing.
  let notices = app
    .try_state::<crate::overlay::notification::NotificationManager>()
    .unwrap()
    .take_test_requests();
  assert_eq!(notices.len(), 1, "a failed Stop is reported to the user");
  assert!(notices[0].critical, "it bypasses notification preferences");
}
