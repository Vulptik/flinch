//! System shortcut handlers
//!
//! ## PrintScreen Suppression
//! On Windows, the PrintScreen key triggers the default OS screenshot behavior.
//! When Flinch uses PrintScreen for its own shortcuts, we register a no-op handler
//! to suppress the OS default.

use crate::app::context::AppHandle;
use crate::app::hotkeys::{GlobalShortcutExt, Shortcut, ShortcutEvent};

const PRINTSCREEN_SHORTCUT: &str = "PrintScreen";

/// Register bare PrintScreen with a no-op handler to suppress OS default behavior
/// (e.g. Windows copying the screen to clipboard). Only call when PrintScreen is
/// not already assigned to one of our shortcuts.
pub fn register_printscreen_noop(app: &AppHandle) -> Result<(), String> {
  let shortcut: Shortcut = PRINTSCREEN_SHORTCUT
    .parse()
    .map_err(|e| format!("Failed to parse {PRINTSCREEN_SHORTCUT}: {e}"))?;
  let handler = |_app: &AppHandle, _shortcut: &Shortcut, _event: ShortcutEvent| {
    #[cfg(feature = "native-settings")]
    if _event.state == crate::app::hotkeys::ShortcutState::Released
      && let Err(error) = flinch_settings::slint::invoke_from_event_loop(|| {
        crate::settings::native_host::capture_key("PrintScreen".into());
      })
    {
      tracing::error!(%error, "Cannot dispatch captured PrintScreen key");
    }
  };
  app
    .global_shortcut()
    .on_shortcut(shortcut, handler)
    .map_err(|e| format!("Failed to register {PRINTSCREEN_SHORTCUT} no-op: {e}"))?;
  tracing::debug!(
    operation = "register_printscreen_noop",
    "Registered PrintScreen with no-op to suppress OS default"
  );
  Ok(())
}

/// Unregister the bare PrintScreen no-op.
/// Idempotent: if it was not registered, returns Ok(()).
pub fn unregister_printscreen_noop(app: &AppHandle) -> Result<(), String> {
  if let Err(e) = app.global_shortcut().unregister(PRINTSCREEN_SHORTCUT) {
    tracing::debug!(
        operation = "unregister_printscreen_noop",
        error = %e,
        "PrintScreen no-op unregister (may not have been registered)"
    );
    return Ok(()); // Idempotent: treat as success for repeated native calls
  }
  tracing::debug!(
    operation = "unregister_printscreen_noop",
    "Unregistered PrintScreen no-op"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn printscreen_shortcut_constant_parses_successfully() {
    let shortcut: Shortcut = PRINTSCREEN_SHORTCUT
      .parse()
      .expect("PrintScreen shortcut should parse");

    let rendered = shortcut.to_string();
    assert!(
      rendered.contains("PrintScreen"),
      "unexpected shortcut rendering: {rendered}"
    );
  }
}
