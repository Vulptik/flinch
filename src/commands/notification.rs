// src/commands/notification.rs
use crate::app::context::AppHandle;

pub async fn show_notification(app: AppHandle, title: String, body: String) -> Result<(), String> {
  crate::notifications::show(&app, &title, &body);
  Ok(())
}
