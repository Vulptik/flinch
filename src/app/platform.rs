//! Windows services independent of the application host. Blocking calls stay on
//! owned workers; a timed-out caller does not release resources still in use.

use std::{
  path::{Path, PathBuf},
  sync::{Arc, LazyLock},
  time::Duration,
};
use windows::{
  Win32::{
    System::Com::{
      CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
      CoTaskMemFree, CoUninitialize,
    },
    UI::Shell::{
      Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
      FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, IFileDialog,
      IFileOpenDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
      ShellExecuteW,
    },
    UI::WindowsAndMessaging::SW_SHOWNORMAL,
  },
  core::{HSTRING, Interface, PCWSTR},
};

struct Apartment;
impl Apartment {
  fn new() -> anyhow::Result<Self> {
    // SAFETY: this dedicated blocking worker owns its COM apartment.
    unsafe {
      CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    Ok(Self)
  }
}
impl Drop for Apartment {
  fn drop(&mut self) {
    // SAFETY: balances the successful initialization on this same worker.
    unsafe {
      CoUninitialize();
    }
  }
}

pub async fn configuration_picker(export: bool, title: String) -> anyhow::Result<Option<PathBuf>> {
  run_picker(move || pick_configuration(export, &title)).await
}

/// `initial` opens at its nearest existing parent, since a chosen folder may be gone.
pub async fn folder_picker(title: String, initial: PathBuf) -> anyhow::Result<Option<PathBuf>> {
  run_picker(move || pick_folder(&title, &initial)).await
}

async fn run_picker(
  pick: impl FnOnce() -> anyhow::Result<Option<PathBuf>> + Send + 'static,
) -> anyhow::Result<Option<PathBuf>> {
  static PICKER: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
  let permit = tokio::time::timeout(Duration::from_secs(5), PICKER.clone().acquire_owned())
    .await
    .map_err(|_| anyhow::anyhow!("Another native picker is still open"))??;
  tokio::time::timeout(
    Duration::from_secs(300),
    tokio::task::spawn_blocking(move || {
      let _permit = permit;
      pick()
    }),
  )
  .await
  .map_err(|_| {
    anyhow::anyhow!(
      "Native picker exceeded five minutes; the native dialog remains owned by its worker"
    )
  })??
}

fn pick_folder(title: &str, initial: &Path) -> anyhow::Result<Option<PathBuf>> {
  let _apartment = Apartment::new()?;
  // SAFETY: COM is initialized, classes and requested interfaces match, and all
  // interfaces and string buffers live until the dialog finishes on this worker.
  unsafe {
    let open: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
    let dialog: IFileDialog = open.cast()?;
    dialog.SetTitle(&HSTRING::from(title))?;
    dialog.SetOptions(
      dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
    )?;
    if let Some(folder) = initial.ancestors().find(|folder| folder.is_dir()) {
      let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(folder.as_os_str()), None)?;
      dialog.SetFolder(&item)?;
    }
    select_configuration_with(&dialog, |dialog| dialog.Show(None))
  }
}

fn pick_configuration(export: bool, title: &str) -> anyhow::Result<Option<PathBuf>> {
  pick_configuration_with(export, title, |dialog| {
    select_configuration_with(dialog, |dialog| {
      // SAFETY: the worker owns the configured dialog and its COM apartment.
      unsafe { dialog.Show(None) }
    })
  })
}

fn pick_configuration_with(
  export: bool,
  title: &str,
  select: impl FnOnce(&IFileDialog) -> anyhow::Result<Option<PathBuf>>,
) -> anyhow::Result<Option<PathBuf>> {
  let _apartment = Apartment::new()?;
  // SAFETY: COM is initialized, classes and requested interfaces match, and all
  // interfaces and string buffers live until the dialog finishes on this worker.
  unsafe {
    let dialog: IFileDialog = if export {
      let save: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
      save.cast()?
    } else {
      let open: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
      open.cast()?
    };
    let title = HSTRING::from(title);
    dialog.SetTitle(&title)?;
    let name = HSTRING::from("JSON");
    let pattern = HSTRING::from("*.json");
    dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
      pszName: PCWSTR(name.as_ptr()),
      pszSpec: PCWSTR(pattern.as_ptr()),
    }])?;
    dialog.SetDefaultExtension(&HSTRING::from("json"))?;
    let options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    dialog.SetOptions(
      options
        | if export {
          FOS_OVERWRITEPROMPT
        } else {
          FOS_FILEMUSTEXIST
        },
    )?;
    if export {
      dialog.SetFileName(&HSTRING::from("config.json"))?;
    }
    select(&dialog)
  }
}

fn select_configuration_with(
  dialog: &IFileDialog,
  show: impl FnOnce(&IFileDialog) -> windows::core::Result<()>,
) -> anyhow::Result<Option<PathBuf>> {
  // SAFETY: the worker's configured dialog and returned shell item stay alive through path copying.
  unsafe {
    if let Err(error) = show(dialog) {
      // HRESULT_FROM_WIN32(ERROR_CANCELLED), the only non-error dismissal.
      if error.code().0 as u32 == 0x800704c7 {
        return Ok(None);
      }
      return Err(error.into());
    }
    let item = dialog.GetResult()?;
    let allocated = item.GetDisplayName(SIGDN_FILESYSPATH)?;
    let result = allocated.to_string().map(PathBuf::from);
    CoTaskMemFree(Some(allocated.0.cast()));
    Ok(Some(result?))
  }
}

pub async fn open_target(target: String) -> anyhow::Result<()> {
  tokio::task::spawn_blocking(move || {
    let target = HSTRING::from(target);
    // SAFETY: the null-terminated target lives through the call, no parameters
    // or shell command are supplied, and the caller validates allowed targets.
    let status = unsafe {
      ShellExecuteW(
        None,
        &HSTRING::from("open"),
        &target,
        None,
        None,
        SW_SHOWNORMAL,
      )
    };
    let code = status.0 as isize;
    anyhow::ensure!(
      code > 32,
      "Windows could not open the requested target (ShellExecute status {code})"
    );
    Ok(())
  })
  .await?
}

/// Opens Explorer on the folder holding `path`, with the file selected.
pub async fn reveal_in_folder(path: PathBuf) -> anyhow::Result<()> {
  tokio::task::spawn_blocking(move || {
    let argument = HSTRING::from(format!("/select,\"{}\"", path.display()));
    // SAFETY: every string outlives the call; Explorer receives one quoted path argument.
    let status = unsafe {
      ShellExecuteW(
        None,
        &HSTRING::from("open"),
        &HSTRING::from("explorer.exe"),
        &argument,
        None,
        SW_SHOWNORMAL,
      )
    };
    let code = status.0 as isize;
    anyhow::ensure!(
      code > 32,
      "Windows could not show the file in its folder (ShellExecute status {code})"
    );
    Ok(())
  })
  .await?
}

pub fn show_toast(title: &str, body: &str) -> anyhow::Result<()> {
  use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
  let _apartment = Apartment::new()?;
  let document = toast_document(title, body)?;
  #[cfg(feature = "msix")]
  let identity = {
    let package = windows::ApplicationModel::Package::Current()?;
    format!("{}!Flinch", package.Id()?.FamilyName()?)
  };
  #[cfg(not(feature = "msix"))]
  let identity = "io.github.vulptik.flinch".to_owned();
  ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(identity))?
    .Show(&ToastNotification::CreateToastNotification(&document)?)?;
  Ok(())
}

fn toast_document(title: &str, body: &str) -> anyhow::Result<windows::Data::Xml::Dom::XmlDocument> {
  use windows::Data::Xml::Dom::XmlDocument;
  let document = XmlDocument::new()?;
  document.LoadXml(&HSTRING::from(
    r#"<toast><visual><binding template="ToastGeneric"><text/><text/></binding></visual></toast>"#,
  ))?;
  let fields = document.GetElementsByTagName(&HSTRING::from("text"))?;
  // Use XML text nodes so capture filenames and error strings cannot inject markup.
  fields
    .Item(0)?
    .AppendChild(&document.CreateTextNode(&HSTRING::from(title))?)?;
  fields
    .Item(1)?
    .AppendChild(&document.CreateTextNode(&HSTRING::from(body))?)?;
  Ok(document)
}

#[cfg(test)]
mod tests;
