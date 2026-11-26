use std::path::PathBuf;

pub const NOTIFICATION_THUMB_SIZE: u32 = 48;

/// Thumbnail image data (pre-downscaled RGBA pixels).
#[derive(Clone)]
pub struct ThumbnailImage {
  pub rgba: Vec<u8>,
  pub width: u32,
  pub height: u32,
}

/// Action performed when the user clicks the notification toast.
pub enum ClickAction {
  /// Open Explorer with the file selected.
  OpenFolder(PathBuf),
}

/// A notification to display as an overlay toast.
pub struct NotificationRequest {
  pub title: String,
  pub body: String,
  pub thumbnail: Option<ThumbnailImage>,
  pub click_action: Option<ClickAction>,
  /// Whether to play the notification sound (still gated by global `notification_sound` config).
  pub play_sound: bool,
  /// Failures remain visible when routine capture notifications are disabled.
  pub critical: bool,
  /// A transient status becomes obsolete when its producer completes.
  pub completed: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
  pub processing: Option<ProcessingFeedback>,
}

impl NotificationRequest {
  pub fn should_display(&self, notifications_enabled: bool) -> bool {
    !self.is_obsolete() && (self.critical || notifications_enabled)
  }

  pub fn is_obsolete(&self) -> bool {
    self
      .completed
      .as_ref()
      .is_some_and(|done| done.load(std::sync::atomic::Ordering::Acquire))
  }
}

/// Which corner of the screen the toast appears in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotificationPosition {
  TopLeft,
  TopRight,
  BottomLeft,
  BottomRight,
}

impl NotificationPosition {
  pub fn from_config(position: &crate::config::NotificationPosition) -> Self {
    match position {
      crate::config::NotificationPosition::TopLeft => Self::TopLeft,
      crate::config::NotificationPosition::TopRight => Self::TopRight,
      crate::config::NotificationPosition::BottomLeft => Self::BottomLeft,
      crate::config::NotificationPosition::BottomRight => Self::BottomRight,
    }
  }

  /// Returns true if the toast should slide in from the right edge.
  pub fn slides_from_right(&self) -> bool {
    matches!(self, Self::TopRight | Self::BottomRight)
  }

  /// Returns true if the toast is positioned at the bottom.
  pub fn is_bottom(&self) -> bool {
    matches!(self, Self::BottomLeft | Self::BottomRight)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn from_config_maps_supported_values() {
    assert_eq!(
      NotificationPosition::from_config(&crate::config::NotificationPosition::TopLeft),
      NotificationPosition::TopLeft
    );
    assert_eq!(
      NotificationPosition::from_config(&crate::config::NotificationPosition::TopRight),
      NotificationPosition::TopRight
    );
    assert_eq!(
      NotificationPosition::from_config(&crate::config::NotificationPosition::BottomLeft),
      NotificationPosition::BottomLeft
    );
    assert_eq!(
      NotificationPosition::from_config(&crate::config::NotificationPosition::BottomRight),
      NotificationPosition::BottomRight
    );
  }
  #[test]
  fn slides_from_right_matches_right_side_positions() {
    assert!(!NotificationPosition::TopLeft.slides_from_right());
    assert!(NotificationPosition::TopRight.slides_from_right());
    assert!(!NotificationPosition::BottomLeft.slides_from_right());
    assert!(NotificationPosition::BottomRight.slides_from_right());
  }

  #[test]
  fn is_bottom_matches_bottom_positions() {
    assert!(!NotificationPosition::TopLeft.is_bottom());
    assert!(!NotificationPosition::TopRight.is_bottom());
    assert!(NotificationPosition::BottomLeft.is_bottom());
    assert!(NotificationPosition::BottomRight.is_bottom());
  }
}

/// Read by the overlay owner without acquiring the recorder lock.
#[derive(Clone)]
pub struct ProcessingFeedback {
  pub started: std::time::Instant,
  pub stage: std::sync::Arc<std::sync::atomic::AtomicU8>,
  pub labels: Vec<String>,
}
impl ProcessingFeedback {
  pub fn body(&self) -> String {
    let index = usize::from(self.stage.load(std::sync::atomic::Ordering::Acquire));
    let label = self
      .labels
      .get(index)
      .or_else(|| self.labels.first())
      .map(String::as_str)
      .unwrap_or("");
    let seconds = self.started.elapsed().as_secs();
    format!("{label} · {}:{:02}", seconds / 60, seconds % 60)
  }
}
