//! Store packages use a package-owned task, never a versioned WindowsApps Run path.

#[cfg(feature = "msix")]
pub const TASK_ID: &str = "FlinchStartup";

#[cfg(feature = "msix")]
async fn task() -> anyhow::Result<windows::ApplicationModel::StartupTask> {
  use std::future::IntoFuture;
  let operation = windows::ApplicationModel::StartupTask::GetAsync(&TASK_ID.into())?;
  match tokio::time::timeout(
    std::time::Duration::from_secs(10),
    operation.clone().into_future(),
  )
  .await
  {
    Ok(result) => Ok(result?),
    Err(error) => {
      if let Err(cancel_error) = operation.Cancel() {
        tracing::warn!(%cancel_error, "Failed to cancel startup task lookup");
      }
      Err(error.into())
    },
  }
}

#[cfg(feature = "msix")]
fn enabled(state: windows::ApplicationModel::StartupTaskState) -> bool {
  use windows::ApplicationModel::StartupTaskState as State;
  matches!(state, State::Enabled | State::EnabledByPolicy)
}

#[cfg(feature = "msix")]
pub async fn is_enabled() -> anyhow::Result<bool> {
  Ok(enabled(task().await?.State()?))
}

pub async fn set_enabled(app: &crate::app::context::AppHandle, enable: bool) -> anyhow::Result<()> {
  #[cfg(not(feature = "msix"))]
  {
    let blocked = crate::i18n::autostart_blocked(&crate::i18n::get_lang(app));
    tokio::task::spawn_blocking(move || set_unpacked_startup(enable, blocked)).await??;
    Ok(())
  }
  #[cfg(feature = "msix")]
  {
    use std::future::IntoFuture;
    let task = task().await?;
    if enable && !enabled(task.State()?) {
      let operation = task.RequestEnableAsync()?;
      match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        operation.clone().into_future(),
      )
      .await
      {
        Ok(result) => {
          result?;
        },
        Err(error) => {
          if let Err(cancel_error) = operation.Cancel() {
            tracing::warn!(%cancel_error, "Failed to cancel startup enable request");
          }
          return Err(error.into());
        },
      }
    } else if !enable {
      task.Disable()?;
    }
    if enabled(task.State()?) != enable {
      anyhow::bail!(crate::i18n::autostart_blocked(&crate::i18n::get_lang(app)));
    }
    Ok(())
  }
}

#[cfg(not(feature = "msix"))]
fn set_unpacked_startup(enable: bool, blocked: &str) -> anyhow::Result<()> {
  use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE},
  };
  const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
  const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
  // Preserve the registry value used by the former host plugin on upgrades.
  const NAME: &str = "flinch";
  let user = RegKey::predef(HKEY_CURRENT_USER);
  if enable {
    match user.open_subkey_with_flags(APPROVED, KEY_READ) {
      Ok(key) => match key.get_raw_value(NAME) {
        Ok(value) => {
          anyhow::ensure!(
            value.bytes.len() >= 4,
            "Windows startup approval data is incomplete"
          );
          let status = u32::from_le_bytes([
            value.bytes[0],
            value.bytes[1],
            value.bytes[2],
            value.bytes[3],
          ]);
          anyhow::ensure!(matches!(status, 2 | 6), "{blocked}");
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
      },
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
      Err(error) => return Err(error.into()),
    }
    let executable = std::env::current_exe()?;
    let command = startup_command(&executable)?;
    let (key, _) = user.create_subkey(RUN)?;
    key.set_value(NAME, &command)?;
    anyhow::ensure!(
      key.get_value::<String, _>(NAME)? == command,
      "Windows did not retain the startup registration"
    );
  } else {
    match user.open_subkey_with_flags(RUN, KEY_SET_VALUE) {
      Ok(key) => match key.delete_value(NAME) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
      },
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
      Err(error) => return Err(error.into()),
    }
  }
  Ok(())
}

#[cfg(not(feature = "msix"))]
fn startup_command(executable: &std::path::Path) -> anyhow::Result<String> {
  let path = executable
    .to_str()
    .ok_or_else(|| anyhow::anyhow!("Startup executable path is not valid Unicode"))?;
  anyhow::ensure!(
    executable.is_absolute() && !path.contains(['"', '\0', '\r', '\n']),
    "Startup executable path is invalid"
  );
  Ok(format!("\"{path}\""))
}

#[cfg(all(test, not(feature = "msix")))]
mod unpacked_tests {
  use super::*;
  #[test]
  fn startup_quotes_spaces_and_rejects_injected_arguments() {
    assert_eq!(
      startup_command(std::path::Path::new(r"C:\Program Files\Flinch\flinch.exe"))
        .expect("absolute path"),
      r#""C:\Program Files\Flinch\flinch.exe""#
    );
    for path in [
      "relative.exe",
      "C:\\flinch.exe\" --other",
      "C:\\flinch.exe\n",
    ] {
      assert!(startup_command(std::path::Path::new(path)).is_err());
    }
  }
}

#[cfg(all(test, feature = "msix"))]
mod tests {
  use super::*;
  use windows::ApplicationModel::StartupTaskState as State;

  #[test]
  fn windows_startup_states_are_authoritative() {
    assert!(enabled(State::Enabled));
    assert!(enabled(State::EnabledByPolicy));
    for state in [
      State::Disabled,
      State::DisabledByUser,
      State::DisabledByPolicy,
      State(99),
    ] {
      assert!(!enabled(state));
    }
  }
}
