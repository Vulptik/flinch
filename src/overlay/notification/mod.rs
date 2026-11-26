mod manager;
mod renderer;
mod sound;
pub mod thumbnail;
mod types;

pub use manager::NotificationManager;
/// The offscreen replay renders its toasts with the production layout.
#[cfg(test)]
pub(crate) use renderer::render_toast;
pub use thumbnail::build_thumbnail_image;
pub use types::{
  ClickAction, NOTIFICATION_THUMB_SIZE, NotificationPosition, NotificationRequest,
  ProcessingFeedback, ThumbnailImage,
};
