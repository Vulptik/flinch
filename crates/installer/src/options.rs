//! Installation choices; never write app preferences or installation state here.
pub fn validate_directory(value: &str) -> Result<(), String> {
  let bytes = value.as_bytes();
  if bytes.len() < 4
    || !bytes[0].is_ascii_alphabetic()
    || bytes[1] != b':'
    || bytes[2] != b'\\'
    || value.len() > 220
    || value
      .chars()
      .any(|c| c.is_control() || "<>\"|?*".contains(c))
    || value[3..].contains(':')
    || value[3..]
      .split(['\\', '/'])
      .any(|part| part == "." || part == ".." || part.ends_with([' ', '.']))
  {
    return Err("Invalid installation directory".into());
  }
  Ok(())
}

#[cfg(feature = "ui")]
pub mod native {
  use windows::{
    Win32::{
      Foundation::{ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HWND},
      System::{
        Com::{
          CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
          CoTaskMemFree, CoUninitialize,
        },
        Registry::{
          HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
          RRF_SUBKEY_WOW6464KEY, RegGetValueW,
        },
      },
      UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_ProgramFilesX64, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
        FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        SIGDN_FILESYSPATH,
      },
    },
    core::HSTRING,
  };
  const KEY: &str = "Software\\FlinchInstaller";
  pub struct Choices {
    pub machine: bool,
    pub directory: String,
    pub start_menu: bool,
    pub desktop: bool,
    pub locked: bool,
  }

  pub fn default_directory(machine: bool) -> Result<String, String> {
    // SAFETY: shell allocates a terminated UTF-16 path; free it after copying.
    let path = unsafe {
      SHGetKnownFolderPath(
        if machine {
          &FOLDERID_ProgramFilesX64
        } else {
          &FOLDERID_LocalAppData
        },
        KF_FLAG_DEFAULT,
        None,
      )
    }
    .map_err(|e| format!("Resolve installation directory: {e}"))?;
    // SAFETY: the shell returned a live NUL-terminated string.
    let result =
      unsafe { path.to_string() }.map_err(|e| format!("Decode installation directory: {e}"));
    // SAFETY: this exact allocation was returned by the shell.
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    let base = std::path::PathBuf::from(result?);
    Ok(
      if machine { base } else { base.join("Programs") }
        .join("Flinch")
        .to_string_lossy()
        .into_owned(),
    )
  }

  fn read(root: HKEY, name: &str, string: bool) -> Result<Option<Vec<u8>>, String> {
    let key = HSTRING::from(KEY);
    let name = HSTRING::from(name);
    let mut size = 65536u32;
    let mut data = vec![0u8; size as usize];
    // SAFETY: key/name are terminated, the initialized buffer matches the supplied capacity.
    let status = unsafe {
      RegGetValueW(
        root,
        &key,
        &name,
        (if string {
          RRF_RT_REG_SZ
        } else {
          RRF_RT_REG_DWORD
        }) | RRF_SUBKEY_WOW6464KEY,
        None,
        Some(data.as_mut_ptr().cast()),
        Some(&mut size),
      )
    };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
      return Ok(None);
    }
    if status.0 != 0 {
      return Err(format!("Read installer settings: {}", status.0));
    }
    data.truncate(size as usize);
    Ok(Some(data))
  }
  pub fn load() -> Result<Choices, String> {
    let user = read(HKEY_CURRENT_USER, "InstallDir", true)?;
    let machine = read(HKEY_LOCAL_MACHINE, "InstallDir", true)?;
    let legacy = read(HKEY_LOCAL_MACHINE, "MenuShortcut", false)?.is_some();
    if user.is_some() && (machine.is_some() || legacy) {
      return Err("Both user and machine installations exist; remove one before continuing".into());
    }
    let is_machine = machine.is_some() || legacy;
    let locked = user.is_some() || is_machine;
    let directory = match if is_machine { machine } else { user } {
      Some(data) => {
        let words: Vec<u16> = data
          .chunks_exact(2)
          .map(|p| u16::from_le_bytes([p[0], p[1]]))
          .take_while(|c| *c != 0)
          .collect();
        String::from_utf16(&words).map_err(|e| format!("Read installed folder: {e}"))?
      },
      None => default_directory(is_machine)?,
    };
    let root = if is_machine {
      HKEY_LOCAL_MACHINE
    } else {
      HKEY_CURRENT_USER
    };
    let flag = |name, default| -> Result<bool, String> {
      Ok(read(root, name, false)?.map_or(default, |v| v.first() == Some(&1)))
    };
    Ok(Choices {
      machine: is_machine,
      directory,
      start_menu: flag("MenuShortcut", false)?,
      desktop: flag("DesktopShortcut", false)?,
      locked,
    })
  }

  pub fn browse(parent: HWND, title: &str) -> Result<Option<String>, String> {
    // SAFETY: the modal shell dialog and apartment lifetime remain on the Slint UI thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
      .ok()
      .map_err(|e| format!("Initialize folder dialog: {e}"))?;
    struct Apartment;
    impl Drop for Apartment {
      fn drop(&mut self) {
        // SAFETY: balances this function's successful CoInitializeEx call.
        unsafe { CoUninitialize() };
      }
    }
    let _apartment = Apartment;
    let result = (|| -> windows::core::Result<Option<String>> {
      // SAFETY: create/use the shell dialog in the initialized apartment with a live parent.
      unsafe {
        let dialog: IFileOpenDialog =
          CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)?;
        dialog.SetTitle(&HSTRING::from(title))?;
        if let Err(error) = dialog.Show(Some(parent)) {
          if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
            return Ok(None);
          }
          return Err(error);
        }
        let item = dialog.GetResult()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let value = path.to_string();
        CoTaskMemFree(Some(path.0.cast()));
        Ok(Some(value?))
      }
    })();
    result.map_err(|e| format!("Select installation directory: {e}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn folder_requires_an_absolute_non_root_windows_path() {
    for value in [
      "",
      "C:\\",
      "relative",
      "C:\\Apps\\..\\Windows",
      "C:\\bad\"folder",
      "C:\\bad.",
    ] {
      assert!(validate_directory(value).is_err(), "{value}");
    }
    for value in [
      "C:\\Users\\Account\\AppData\\Local\\Programs\\Flinch",
      "D:\\Applications\\Flinch",
      "C:\\Applications\\Écran",
    ] {
      assert!(validate_directory(value).is_ok(), "{value}");
    }
  }
}
