use super::{context::AppHandle, instance::Instance};
pub fn build_app() -> anyhow::Result<AppHandle> {
  let executable = std::env::current_exe()?;
  let resources = executable
    .parent()
    .ok_or_else(|| anyhow::anyhow!("Application executable has no directory"))?
    .to_owned();
  #[cfg(debug_assertions)]
  let config =
    crate::settings::native_probe::configuration().unwrap_or_else(crate::config::Config::load);
  #[cfg(not(debug_assertions))]
  let config = crate::config::Config::load();
  Ok(AppHandle::new(config, resources))
}
pub fn activate(app: &AppHandle) -> anyhow::Result<Option<Instance>> {
  if crate::settings::native_host::is_probe() {
    return Ok(None);
  }
  let handle = app.clone();
  let shutdown_handle = app.clone();
  let instance = match Instance::acquire(
    crate::probe_profile::identity_suffix(),
    move || {
      if let Err(error) = crate::settings::native_host::show(&handle, None) {
        tracing::error!(%error, "Cannot show settings for second instance");
      }
    },
    move || crate::settings::native_host::request_exit(&shutdown_handle),
  )? {
    Some(value) => value,
    None => std::process::exit(0),
  };
  if let Err(error) = crate::config::Config::back_up_for_current_version() {
    tracing::warn!(error = %format!("{error:#}"), "Cannot back up settings for this version");
  }
  // A settings reset or restore must be visible even if settings is never opened.
  crate::commands::config::show_pending_load_warning();
  crate::screenshot::warm_up_desktop_duplication();
  crate::overlay::magnifier::warm_up();
  crate::overlay::chrome::warm_up();
  super::hotkeys::initialize().map_err(anyhow::Error::msg)?;
  super::setup_tray_icon(app).map_err(|error| anyhow::anyhow!(error.to_string()))?;
  crate::shortcuts::setup_global_shortcuts_delayed(app, 250);
  if let Err(error) = crate::commands::video::start::cleanup_orphaned_recording_temp_files(app) {
    tracing::warn!(%error, "Cannot clean interrupted recording temporary files");
  }
  crate::commands::video::start::recover_interrupted_recordings(app);
  Ok(Some(instance))
}
pub fn shutdown(app: &AppHandle, instance: Option<Instance>) {
  if let Some(manager) = app.try_state::<crate::overlay::notification::NotificationManager>() {
    manager.shutdown();
  }
  if !crate::settings::native_host::is_probe() {
    super::hotkeys::shutdown();
    super::tray::shutdown();
  }
  if let Some(instance) = instance {
    instance.shutdown();
  }
}
