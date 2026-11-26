use std::path::Path;

use crate::app::context::AppHandle;

use crate::config::Config;

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingLifecycleStatus {
  Idle,
  Processing,
  Recording,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CaptureResponse {
  pub success: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub status: Option<RecordingLifecycleStatus>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub saved: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub message: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub cancelled: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub path: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source_app: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub width: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub height: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub duration: Option<u64>,
}

impl CaptureResponse {
  pub fn saved(path: Option<&Path>) -> Self {
    Self {
      success: true,
      status: None,
      saved: Some(true),
      message: None,
      cancelled: None,
      path: path.map(path_string),
      url: None,
      source_app: None,
      width: None,
      height: None,
      duration: None,
    }
  }

  pub fn cancelled() -> Self {
    Self {
      success: false,
      status: None,
      saved: None,
      message: None,
      cancelled: Some(true),
      path: None,
      url: None,
      source_app: None,
      width: None,
      height: None,
      duration: None,
    }
  }

  pub fn processing(message: impl Into<String>) -> Self {
    Self {
      success: false,
      status: Some(RecordingLifecycleStatus::Processing),
      saved: None,
      message: Some(message.into()),
      cancelled: None,
      path: None,
      url: None,
      source_app: None,
      width: None,
      height: None,
      duration: None,
    }
  }

  pub fn with_source_app(mut self, source_app: Option<&str>) -> Self {
    self.source_app = source_app.map(str::to_string);
    self
  }

  pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
    self.width = Some(width);
    self.height = Some(height);
    self
  }

  pub fn with_duration(mut self, duration: u64) -> Self {
    self.duration = Some(duration);
    self
  }
}

#[allow(clippy::too_many_arguments)]
pub async fn finalize_saved_artifact_with_notifications(
  app: &AppHandle,
  _config: &Config,
  artifact_type: &'static str,
  path: &Path,
  _operation_name: &str,
  saved_title: &str,
  saved_body: &str,
) {
  crate::recent_captures::record(
    app,
    path,
    if artifact_type == "gif" {
      crate::recent_captures::CaptureKind::Gif
    } else {
      crate::recent_captures::CaptureKind::Recording
    },
  );
  // Like screenshots, a saved video or GIF goes straight to the clipboard, as a file: only its
  // path, so any size is instant. If that fails the notification says where it was saved.
  let copied =
    match crate::commands::clipboard::copy_file_to_clipboard_with_retry(app, path.to_path_buf())
      .await
    {
      Ok(()) => true,
      Err(error) => {
        tracing::warn!(%error, artifact_type, "Cannot copy the saved recording to the clipboard");
        false
      },
    };
  let body = if copied {
    crate::i18n::tray(&crate::i18n::get_lang(app)).recent_copied
  } else {
    saved_body
  };
  crate::notifications::show_file(app, saved_title, body, path.to_path_buf()).await;
}

pub fn build_capture_response(path: Option<&Path>) -> CaptureResponse {
  CaptureResponse::saved(path)
}

pub fn path_string(path: &Path) -> String {
  path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The isolated app has no clipboard, so the copy fails without touching the system one and the
  /// notification still says where the recording was saved.
  #[tokio::test]
  async fn a_recording_that_cannot_be_copied_still_notifies_where_it_was_saved() {
    let folder = tempfile::tempdir().unwrap();
    let app = AppHandle::isolated(
      Config {
        language: "en".into(),
        ..Default::default()
      },
      std::path::PathBuf::new(),
    );
    app.isolate_config_path(folder.path().join("config.json"));
    let path = folder.path().join("clip.mp4");
    std::fs::write(&path, b"mp4").unwrap();
    finalize_saved_artifact_with_notifications(
      &app,
      &Config::default(),
      "video",
      &path,
      "test",
      "Video saved",
      "Saved to: clip.mp4",
    )
    .await;
    let requests = app
      .try_state::<crate::overlay::notification::NotificationManager>()
      .unwrap()
      .take_test_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].title, "Video saved");
    assert_eq!(requests[0].body, "Saved to: clip.mp4");
    assert!(
      !requests[0].critical,
      "a clipboard failure is not a save failure"
    );
    assert_eq!(crate::recent_captures::list(&app).len(), 1);
  }
}
