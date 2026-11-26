// src/commands/clipboard.rs
// Clipboard operations with retry logic

use super::ClipboardState;
use crate::app::context::AppHandle;
use std::path::Path;
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFOHEADER};
#[cfg(target_os = "windows")]
use windows::Win32::System::DataExchange::{
  CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
#[cfg(target_os = "windows")]
use windows::Win32::UI::Shell::DROPFILES;

#[cfg(target_os = "windows")]
const CF_DIB_FORMAT: u32 = 8;
#[cfg(target_os = "windows")]
const CF_HDROP_FORMAT: u32 = 15;
const CLIPBOARD_MAX_RETRIES: u32 = 5;
/// Screenshots up to this many pixel bytes go on the clipboard as the image and the file; larger
/// ones as the file only. Windows and every app that reads a clipboard image keep it in memory,
/// often converted more than once, so a huge one costs hundreds of megabytes each time, and many
/// apps refuse it anyway. 256 MiB is about 67 megapixels, beyond three 4K screens side by side.
const CLIPBOARD_IMAGE_LIMIT: usize = 256 * 1024 * 1024;
const CLIPBOARD_RETRY_DELAY_MS: u64 = 300;

fn run_clipboard_retry<T, F>(operation: &'static str, attempt: F) -> Result<(T, u64, u32), String>
where
  F: FnMut(u32) -> Result<T, String>,
{
  run_clipboard_retry_with_delay(operation, attempt, std::thread::sleep)
}

fn run_clipboard_retry_with_delay<T>(
  operation: &'static str,
  mut attempt: impl FnMut(u32) -> Result<T, String>,
  mut delay: impl FnMut(std::time::Duration),
) -> Result<(T, u64, u32), String> {
  let start_time = std::time::Instant::now();
  let mut last_error = format!("{operation} failed");

  for retry_count in 0..CLIPBOARD_MAX_RETRIES {
    let attempt_number = retry_count + 1;

    if retry_count > 0 {
      let delay_ms = CLIPBOARD_RETRY_DELAY_MS * retry_count as u64;
      tracing::debug!(
        operation,
        attempt = attempt_number,
        max_attempts = CLIPBOARD_MAX_RETRIES,
        delay_ms,
        "Retrying clipboard operation"
      );
      delay(std::time::Duration::from_millis(delay_ms));
    }

    match attempt(attempt_number) {
      Ok(value) => {
        let latency_ms = start_time.elapsed().as_millis() as u64;
        return Ok((value, latency_ms, attempt_number));
      },
      Err(error) => {
        tracing::warn!(
          operation,
          attempt = attempt_number,
          error = %error,
          "Clipboard operation attempt failed"
        );
        last_error = error;
      },
    }
  }

  let final_error = format!("{} (after {} attempts)", last_error, CLIPBOARD_MAX_RETRIES);
  let latency_ms = start_time.elapsed().as_millis() as u64;
  tracing::error!(
    operation,
    max_attempts = CLIPBOARD_MAX_RETRIES,
    latency_ms,
    error = %final_error,
    "Clipboard operation failed after all retries"
  );
  Err(final_error)
}

pub(crate) async fn copy_text_to_clipboard_with_retry(
  app: &AppHandle,
  text: String,
) -> Result<(), String> {
  let app_clone = app.clone();
  tokio::task::spawn_blocking(move || {
    let clipboard_state = app_clone
      .try_state::<ClipboardState>()
      .ok_or_else(|| "Clipboard is unavailable".to_owned())?;
    run_clipboard_retry("clipboard_text_copy", |_attempt_number| {
      let mut clipboard_guard =
        clipboard_state.inner().0.lock().map_err(|e| {
          crate::commands::ipc_display_context("Failed to acquire clipboard lock", e)
        })?;
      clipboard_guard.set_text(text.as_str()).map_err(|error| {
        crate::commands::ipc_display_context("Failed to set clipboard text", error)
      })
    })?;
    Ok(())
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Clipboard task panicked", e))?
}

/// Whether a screenshot this size goes on the clipboard as an image as well as its file.
pub(crate) fn fits_clipboard_image(width: u32, height: u32) -> bool {
  (width as usize)
    .checked_mul(height as usize)
    .and_then(|pixels| pixels.checked_mul(4))
    .is_some_and(|bytes| bytes <= CLIPBOARD_IMAGE_LIMIT)
}

/// A saved screenshot: the image and its file when it fits, otherwise, or when the image cannot
/// be placed, the file alone, as videos and GIFs are copied.
pub(crate) async fn copy_screenshot_to_clipboard_with_retry(
  app: &AppHandle,
  width: u32,
  height: u32,
  rgba: std::sync::Arc<Vec<u8>>,
  file_path: std::path::PathBuf,
) -> Result<(), String> {
  image_or_file(
    fits_clipboard_image(width, height),
    || copy_rgba_and_file_to_clipboard_with_retry(app, width, height, rgba, file_path.clone()),
    || copy_file_to_clipboard_with_retry(app, file_path.clone()),
  )
  .await
}

async fn image_or_file<I, F>(
  fits: bool,
  image: impl FnOnce() -> I,
  file: impl FnOnce() -> F,
) -> Result<(), String>
where
  I: std::future::Future<Output = Result<(), String>>,
  F: std::future::Future<Output = Result<(), String>>,
{
  if !fits {
    tracing::info!("Screenshot is too large to copy as an image; copying its file");
    return file().await;
  }
  match image().await {
    Ok(()) => Ok(()),
    Err(error) => {
      tracing::warn!(%error, "Cannot copy the screenshot as an image; copying its file");
      file().await
    },
  }
}

pub(crate) async fn copy_rgba_and_file_to_clipboard_with_retry(
  app: &AppHandle,
  width: u32,
  height: u32,
  rgba: std::sync::Arc<Vec<u8>>,
  file_path: std::path::PathBuf,
) -> Result<(), String> {
  let app_clone = app.clone();

  tokio::task::spawn_blocking(move || {
    let clipboard_state = app_clone
      .try_state::<ClipboardState>()
      .ok_or_else(|| "Clipboard is unavailable".to_owned())?;

    let width_usize = width as usize;
    let height_usize = height as usize;
    let ((), latency_ms, attempt_number) =
      run_clipboard_retry("clipboard_rgba_file_copy", |_attempt_number| {
        let mut clipboard_guard = clipboard_state.inner().0.lock().map_err(|e| {
          crate::commands::ipc_display_context("Failed to acquire clipboard lock", e)
        })?;

        set_image_file_clipboard_contents(
          &mut clipboard_guard,
          width_usize,
          height_usize,
          &rgba,
          &file_path,
        )
        .map_err(|error| {
          crate::commands::ipc_display_context("Failed to set clipboard image/file", error)
        })?;

        Ok(())
      })?;

    tracing::info!(
      operation = "clipboard_rgba_file_copy",
      attempt = attempt_number,
      latency_ms,
      width = width,
      height = height,
      path = %file_path.display(),
      "RGBA+file clipboard copy successful"
    );
    Ok(())
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Clipboard task panicked", e))?
}

fn set_image_file_clipboard_contents(
  clipboard: &mut arboard::Clipboard,
  width: usize,
  height: usize,
  rgba: &[u8],
  file_path: &Path,
) -> Result<(), String> {
  #[cfg(target_os = "windows")]
  {
    let _ = clipboard;
    set_image_file_clipboard_contents_windows(width, height, rgba, file_path)
  }

  #[cfg(not(target_os = "windows"))]
  {
    let _ = file_path;
    let image_data = arboard::ImageData {
      width,
      height,
      bytes: std::borrow::Cow::Borrowed(rgba),
    };
    clipboard
      .set_image(image_data)
      .map_err(|error| crate::commands::ipc_display_context("Failed to set clipboard image", error))
  }
}

#[cfg(target_os = "windows")]
fn set_image_file_clipboard_contents_windows(
  width: usize,
  height: usize,
  rgba: &[u8],
  file_path: &Path,
) -> Result<(), String> {
  let image = OwnedGlobal(alloc_dib_handle(width, height, rgba)?);
  let file = OwnedGlobal(alloc_hdrop_handle(file_path)?);
  publish_image_file(image, file, &NativeClipboard)
}

#[cfg(target_os = "windows")]
struct OwnedGlobal(HGLOBAL);

#[cfg(target_os = "windows")]
impl Drop for OwnedGlobal {
  fn drop(&mut self) {
    if !self.0.0.is_null() {
      // SAFETY: this handle is owned until SetClipboardData accepts it.
      if let Err(error) = unsafe { GlobalFree(Some(self.0)) } {
        tracing::warn!(%error, "Cannot release untransferred clipboard allocation");
      }
    }
  }
}

#[cfg(target_os = "windows")]
trait ClipboardApi {
  fn open(&self) -> Result<(), String>;
  fn empty(&self) -> Result<(), String>;
  fn set(&self, format: u32, handle: HGLOBAL) -> Result<(), String>;
  fn close(&self) -> Result<(), String>;
}

#[cfg(target_os = "windows")]
struct NativeClipboard;

#[cfg(target_os = "windows")]
impl ClipboardApi for NativeClipboard {
  fn open(&self) -> Result<(), String> {
    // SAFETY: the clipboard is opened on this worker and closed before it exits.
    unsafe { OpenClipboard(None) }
      .map_err(|e| crate::commands::ipc_display_context("OpenClipboard failed", e))
  }
  fn empty(&self) -> Result<(), String> {
    // SAFETY: called only while this worker owns the open clipboard.
    unsafe { EmptyClipboard() }
      .map_err(|e| crate::commands::ipc_display_context("EmptyClipboard failed", e))
  }
  fn set(&self, format: u32, handle: HGLOBAL) -> Result<(), String> {
    // SAFETY: movable global memory is initialized, unlocked and owned by the caller.
    unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) }
      .map(|_| ())
      .map_err(|e| {
        crate::commands::ipc_display_context(&format!("SetClipboardData({format}) failed"), e)
      })
  }
  fn close(&self) -> Result<(), String> {
    // SAFETY: balances one successful OpenClipboard call on the same worker.
    unsafe { CloseClipboard() }
      .map_err(|e| crate::commands::ipc_display_context("CloseClipboard failed", e))
  }
}

#[cfg(target_os = "windows")]
fn publish_image_file(
  mut image: OwnedGlobal,
  mut file: OwnedGlobal,
  api: &impl ClipboardApi,
) -> Result<(), String> {
  api.open()?;
  let result = (|| {
    api.empty()?;
    api.set(CF_DIB_FORMAT, image.0)?;
    image.0 = HGLOBAL::default();
    api.set(CF_HDROP_FORMAT, file.0)?;
    file.0 = HGLOBAL::default();
    Ok(())
  })();
  match (result, api.close()) {
    (Err(error), Err(close)) => Err(format!("{error}; {close}")),
    (Err(error), _) | (_, Err(error)) => Err(error),
    (Ok(()), Ok(())) => Ok(()),
  }
}

#[cfg(target_os = "windows")]
fn alloc_dib_handle(width: usize, height: usize, rgba: &[u8]) -> Result<HGLOBAL, String> {
  if width == 0 || height == 0 || width > i32::MAX as usize || height > i32::MAX as usize {
    return Err("Clipboard image dimensions must fit positive Windows bitmap dimensions".into());
  }
  let pixel_bytes = width
    .checked_mul(height)
    .and_then(|px| px.checked_mul(4))
    .ok_or_else(|| "Clipboard image dimensions overflow".to_string())?;
  if pixel_bytes > u32::MAX as usize {
    return Err("Clipboard bitmap byte size exceeds the Windows header limit".into());
  }

  if rgba.len() != pixel_bytes {
    return Err(format!(
      "Clipboard image buffer size mismatch: expected {} bytes, got {}",
      pixel_bytes,
      rgba.len()
    ));
  }

  let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
  let total_bytes = header_size
    .checked_add(pixel_bytes)
    .ok_or_else(|| "Clipboard allocation size overflow".to_string())?;

  unsafe {
    let handle = GlobalAlloc(GMEM_MOVEABLE, total_bytes).map_err(|e| {
      crate::commands::ipc_display_context("GlobalAlloc failed for clipboard image", e)
    })?;

    let locked = GlobalLock(handle);
    if locked.is_null() {
      let _ = GlobalFree(Some(handle));
      return Err("GlobalLock failed for clipboard image".to_string());
    }

    let header_ptr = locked as *mut BITMAPINFOHEADER;
    std::ptr::write(
      header_ptr,
      BITMAPINFOHEADER {
        biSize: header_size as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: pixel_bytes as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
      },
    );

    let pixels_ptr = (locked as *mut u8).add(header_size);
    let pixels = std::slice::from_raw_parts_mut(pixels_ptr, pixel_bytes);
    pixels.copy_from_slice(rgba);
    for chunk in pixels.as_chunks_mut::<4>().0 {
      chunk.swap(0, 2);
    }

    let _ = GlobalUnlock(handle);
    Ok(handle)
  }
}

/// Puts a saved capture on the clipboard as a file, as Explorer's Copy does: only its path, so any
/// size is instant, ready to paste into chats, uploads and folders.
#[cfg(target_os = "windows")]
pub(crate) async fn copy_file_to_clipboard_with_retry(
  app: &AppHandle,
  file_path: std::path::PathBuf,
) -> Result<(), String> {
  let app_clone = app.clone();
  tokio::task::spawn_blocking(move || {
    let clipboard_state = app_clone
      .try_state::<ClipboardState>()
      .ok_or_else(|| "Clipboard is unavailable".to_owned())?;
    run_clipboard_retry("clipboard_file_copy", |_attempt_number| {
      // Held so arboard users on other threads never interleave with this native write.
      let _clipboard_guard =
        clipboard_state.inner().0.lock().map_err(|e| {
          crate::commands::ipc_display_context("Failed to acquire clipboard lock", e)
        })?;
      let file = OwnedGlobal(alloc_hdrop_handle(&file_path)?);
      publish_file(file, &NativeClipboard)
    })?;
    Ok(())
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Clipboard task panicked", e))?
}

/// A pinned screenshot: the image alone, since it has no file. Too large an image is refused,
/// for the reasons `CLIPBOARD_IMAGE_LIMIT` gives.
#[cfg(target_os = "windows")]
pub(crate) async fn copy_rgba_to_clipboard_with_retry(
  app: &AppHandle,
  width: u32,
  height: u32,
  rgba: std::sync::Arc<Vec<u8>>,
) -> Result<(), String> {
  if !fits_clipboard_image(width, height) {
    return Err(format!(
      "A {width}x{height} image is too large for the clipboard"
    ));
  }
  let app_clone = app.clone();
  tokio::task::spawn_blocking(move || {
    let clipboard_state = app_clone
      .try_state::<ClipboardState>()
      .ok_or_else(|| "Clipboard is unavailable".to_owned())?;
    run_clipboard_retry("clipboard_rgba_copy", |_attempt_number| {
      // Held so arboard users on other threads never interleave with this native write.
      let _clipboard_guard =
        clipboard_state.inner().0.lock().map_err(|e| {
          crate::commands::ipc_display_context("Failed to acquire clipboard lock", e)
        })?;
      let image = OwnedGlobal(alloc_dib_handle(width as usize, height as usize, &rgba)?);
      publish_one(CF_DIB_FORMAT, image, &NativeClipboard)
    })?;
    Ok(())
  })
  .await
  .map_err(|e| crate::commands::ipc_context("Clipboard task panicked", e))?
}

#[cfg(target_os = "windows")]
fn publish_file(file: OwnedGlobal, api: &impl ClipboardApi) -> Result<(), String> {
  publish_one(CF_HDROP_FORMAT, file, api)
}

#[cfg(target_os = "windows")]
fn publish_one(format: u32, mut data: OwnedGlobal, api: &impl ClipboardApi) -> Result<(), String> {
  api.open()?;
  let result = (|| {
    api.empty()?;
    api.set(format, data.0)?;
    data.0 = HGLOBAL::default();
    Ok(())
  })();
  match (result, api.close()) {
    (Err(error), Err(close)) => Err(format!("{error}; {close}")),
    (Err(error), _) | (_, Err(error)) => Err(error),
    (Ok(()), Ok(())) => Ok(()),
  }
}

#[cfg(target_os = "windows")]
fn alloc_hdrop_handle(file_path: &Path) -> Result<HGLOBAL, String> {
  use std::os::windows::ffi::OsStrExt;

  let encoded_path: Vec<u16> = file_path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .chain(std::iter::once(0))
    .collect();
  if encoded_path.len() <= 2 || encoded_path[..encoded_path.len() - 2].contains(&0) {
    return Err("Clipboard file path must be nonempty and cannot contain NUL".into());
  }

  let header_size = std::mem::size_of::<DROPFILES>();
  let path_bytes = encoded_path
    .len()
    .checked_mul(std::mem::size_of::<u16>())
    .ok_or_else(|| "Clipboard file path size overflow".to_string())?;
  let total_bytes = header_size
    .checked_add(path_bytes)
    .ok_or_else(|| "Clipboard file allocation size overflow".to_string())?;

  unsafe {
    let handle = GlobalAlloc(GMEM_MOVEABLE, total_bytes).map_err(|e| {
      crate::commands::ipc_display_context("GlobalAlloc failed for clipboard file", e)
    })?;

    let locked = GlobalLock(handle);
    if locked.is_null() {
      let _ = GlobalFree(Some(handle));
      return Err("GlobalLock failed for clipboard file".to_string());
    }

    let header_ptr = locked as *mut DROPFILES;
    std::ptr::write(
      header_ptr,
      DROPFILES {
        pFiles: header_size as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: false.into(),
        fWide: true.into(),
      },
    );

    let paths_ptr = (locked as *mut u8).add(header_size) as *mut u16;
    std::ptr::copy_nonoverlapping(encoded_path.as_ptr(), paths_ptr, encoded_path.len());

    let _ = GlobalUnlock(handle);
    Ok(handle)
  }
}

#[cfg(test)]
mod quiet_tests;

#[cfg(test)]
mod tests {
  use super::*;
  use arboard::Clipboard;
  use serial_test::serial;
  use std::sync::Mutex;
  use std::time::{Duration, SystemTime, UNIX_EPOCH};

  // Explicit desktop tests: these operations overwrite the shared system clipboard.
  // Tests are run serially because clipboard is a shared system resource.

  fn unique_clipboard_text(prefix: &str) -> String {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|duration| duration.as_nanos())
      .unwrap_or(0);
    format!("{prefix}-{nanos}")
  }

  fn assert_clipboard_text_eventually(clipboard: &mut Clipboard, expected: &str) {
    let mut last_text = None;
    let mut last_error = None;

    for _ in 0..10 {
      match clipboard.get_text() {
        Ok(text) if text == expected => return,
        Ok(text) => {
          last_text = Some(text);
          last_error = None;
        },
        Err(error) => {
          last_error = Some(error.to_string());
        },
      }

      std::thread::sleep(Duration::from_millis(50));
    }

    if let Some(text) = last_text {
      panic!("Clipboard never contained expected text. expected={expected:?}, last_seen={text:?}");
    }

    if let Some(error) = last_error {
      panic!("Clipboard never returned expected text. expected={expected:?}, last_error={error}");
    }

    panic!("Clipboard did not return text for expected value {expected:?}");
  }

  /// Test basic clipboard text operations
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_basic_text() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    let text = unique_clipboard_text("test-clipboard-text");
    clipboard
      .set_text(&text)
      .expect("Failed to set clipboard text");

    assert_clipboard_text_eventually(&mut clipboard, &text);
  }

  /// Test empty text
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_empty_text() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    let text = String::new();
    let result = clipboard.set_text(&text);
    // Empty text is valid for clipboard
    result.expect("Empty clipboard text must be accepted");
  }

  /// Test large text
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_large_text() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    // Create a large text string (1MB)
    let text = "x".repeat(1024 * 1024);
    let result = clipboard.set_text(&text);

    if result.is_ok() {
      let clipboard_text = clipboard.get_text().expect("Failed to get clipboard text");
      assert_eq!(
        clipboard_text.len(),
        text.len(),
        "Clipboard should contain full large text"
      );
    }
    // Some platforms may reject very large clipboard content
  }

  /// Test Unicode text
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_unicode() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    let text = format!("Hello 世界 🌍 Émojis {}", unique_clipboard_text("unicode"));
    clipboard
      .set_text(&text)
      .expect("Failed to set Unicode text");

    assert_clipboard_text_eventually(&mut clipboard, &text);
  }

  /// Test RGBA image to clipboard
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_rgba_image() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    let width = 10;
    let height = 10;
    let rgba = vec![255u8; width * height * 4]; // White 10x10 image

    let image_data = arboard::ImageData {
      width,
      height,
      bytes: rgba.into(),
    };

    let result = clipboard.set_image(image_data);
    assert!(result.is_ok(), "RGBA clipboard copy should succeed");
  }

  /// Test sequential clipboard operations
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_sequential_operations() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    // Copy text 1
    let text1 = unique_clipboard_text("first-text");
    clipboard
      .set_text(&text1)
      .expect("Failed to set first text");

    // Copy text 2
    let text2 = unique_clipboard_text("second-text");
    clipboard
      .set_text(&text2)
      .expect("Failed to set second text");

    // Verify latest text is in clipboard
    assert_clipboard_text_eventually(&mut clipboard, &text2);
  }

  /// Test ClipboardState wrapper
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_state_wrapper() {
    let clipboard = Clipboard::new().expect("Failed to create clipboard");
    let state = ClipboardState(Mutex::new(clipboard));

    // Test lock acquisition
    {
      let mut guard = state.0.lock().unwrap();
      guard
        .set_text("test")
        .expect("Failed to set text through state");
    }

    // Lock should be released and reacquirable
    {
      let mut guard = state.0.lock().unwrap();
      assert_clipboard_text_eventually(&mut guard, "test");
    }
  }

  /// Test large RGBA image
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_large_rgba_image() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    let width = 1920;
    let height = 1080;
    // Create gradient pattern
    let rgba: Vec<u8> = (0..(width * height))
      .flat_map(|i| {
        let x = i % width;
        let y = i / width;
        let r = ((x * 255) / width.max(1)) as u8;
        let g = ((y * 255) / height.max(1)) as u8;
        let b = 128u8;
        [r, g, b, 255]
      })
      .collect();

    let image_data = arboard::ImageData {
      width: width as usize,
      height: height as usize,
      bytes: rgba.into(),
    };

    let result = clipboard.set_image(image_data);
    result.expect("Large clipboard image must be accepted");
  }

  /// Test concurrent clipboard access with ClipboardState
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_state_concurrent_access() {
    use std::sync::Arc;
    use std::thread;

    let clipboard = Clipboard::new().expect("Failed to create clipboard");
    let state = Arc::new(ClipboardState(Mutex::new(clipboard)));

    let mut handles = vec![];

    // Spawn multiple threads trying to access clipboard
    for i in 0..5 {
      let state_clone = Arc::clone(&state);
      let handle = thread::spawn(move || {
        let text = format!("thread_{}", i);
        let mut guard = state_clone.0.lock().expect("clipboard state lock");
        guard.set_text(&text).expect("concurrent clipboard write");
      });
      handles.push(handle);
    }

    // Wait for all threads and collect any panics
    let mut panics = Vec::new();
    for (i, handle) in handles.into_iter().enumerate() {
      if let Err(e) = handle.join() {
        panics.push((i, e));
      }
    }

    // Report any panics that occurred
    if !panics.is_empty() {
      panic!(
        "Concurrent clipboard test failed: {} thread(s) panicked: {:?}",
        panics.len(),
        panics
      );
    }

    // If we got here, no deadlocks or panics occurred
  }

  /// Test mixed text and image operations
  #[test]
  #[ignore = "changes the real system clipboard: cargo xtask test:clipboard:desktop"]
  #[serial]
  fn test_clipboard_mixed_operations() {
    let mut clipboard = Clipboard::new().expect("Failed to create clipboard");

    // Set text
    let text = "test text".to_string();
    clipboard.set_text(&text).expect("Failed to set text");

    // Set image
    let width = 5;
    let height = 5;
    let rgba = vec![255u8; width * height * 4];
    let image_data = arboard::ImageData {
      width,
      height,
      bytes: rgba.into(),
    };
    clipboard
      .set_image(image_data)
      .expect("Failed to set image");

    // Both operations should complete without errors
  }
}
