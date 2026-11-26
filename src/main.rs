#![windows_subsystem = "windows"]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
fn main() {
  // The native video engine runs in a child of this same executable; it never starts the app.
  if std::env::args_os().nth(1).as_deref()
    == Some(std::ffi::OsStr::new(flinch::video::native::WORKER_SWITCH))
  {
    let warm = std::env::args_os().nth(2).as_deref()
      == Some(std::ffi::OsStr::new(
        flinch_video_engine::windows::runtime::WARM_ARGUMENT,
      ));
    if let Err(error) = flinch_video_engine::windows::runtime::worker_with(warm) {
      eprintln!("Flinch video worker failed: {error}");
      std::process::exit(1);
    }
    return;
  }
  if let Err(error) = flinch::run() {
    tracing::error!(%error, "Flinch startup or application loop failed");
    eprintln!("Flinch failed: {error:#}");
    let language = flinch::config::Config::load().language;
    let instruction = match flinch::i18n::native_settings(&language, "about_action_error") {
      Ok(value) => value,
      Err(catalog_error) => {
        tracing::error!(%catalog_error, "Cannot localize fatal application dialog");
        "Flinch".to_owned()
      },
    };
    let details = format!("{error:#}");
    let options = flinch::dialogs::TaskDialogOptions::new("Flinch", &instruction, &details)
      .with_icon(flinch::dialogs::TaskDialogIcon::Warning);
    if let Err(dialog_error) = flinch::dialogs::show_task_dialog(&options) {
      tracing::error!(%dialog_error, "Cannot display fatal application error");
    }
    std::process::exit(1);
  }
}
