//! Capture thumbnails from the Windows Shell, the same pictures Explorer shows for PNG, MP4 and
//! GIF. The tray menu uses one worker thread at a time and never waits on it; a saved recording's
//! notification waits briefly for one and goes without if the Shell is slow or has none.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A worker running longer than this is reported once; later requests leave it alone.
const STALL: Duration = Duration::from_secs(5);

/// Premultiplied RGBA pixels, ready for `slint::Image::from_rgba8_premultiplied`.
#[derive(Debug)]
pub(crate) struct Pixels {
  pub width: u32,
  pub height: u32,
  pub rgba: Vec<u8>,
}

struct Worker {
  started: Instant,
  reported: bool,
}

static WORKER: Mutex<Option<Worker>> = Mutex::new(None);
/// The picture the recording engine made of a file it just saved, until that file's notification
/// takes it. One slot: the next recording replaces it, so it never grows.
static REMEMBERED: Mutex<Option<(PathBuf, Pixels)>> = Mutex::new(None);

/// Keep a saved recording's own thumbnail for its notification, so it needs no Shell extraction.
pub(crate) fn remember(path: &Path, pixels: Pixels) {
  match REMEMBERED.lock() {
    Ok(mut slot) => *slot = Some((path.to_path_buf(), pixels)),
    Err(error) => tracing::error!(%error, "Remembered thumbnail state poisoned"),
  }
}

/// The thumbnail remembered for exactly this file, if any; it is handed out once.
pub(crate) fn take_remembered(path: &Path) -> Option<Pixels> {
  match REMEMBERED.lock() {
    Ok(mut slot) => match slot.take() {
      Some((remembered, pixels)) if remembered == path => Some(pixels),
      other => {
        *slot = other;
        None
      },
    },
    Err(error) => {
      tracing::error!(%error, "Remembered thumbnail state poisoned");
      None
    },
  }
}
/// A notification extraction is running; a stuck Shell handler never piles up threads.
static NOTIFICATION_BUSY: AtomicBool = AtomicBool::new(false);

impl Pixels {
  /// Straight-alpha RGBA, as notification thumbnails expect.
  pub fn into_straight(mut self) -> Self {
    unpremultiply(&mut self.rgba);
    self
  }
}

/// One thumbnail, waiting at most `timeout`. Only one runs at a time: while one is still going,
/// even after its caller gave up, later calls return an error at once.
pub(crate) async fn extract_bounded(
  path: PathBuf,
  size: u32,
  timeout: Duration,
) -> Result<Pixels, String> {
  if NOTIFICATION_BUSY.swap(true, Ordering::AcqRel) {
    return Err("A thumbnail is still being extracted".into());
  }
  let (sender, receiver) = tokio::sync::oneshot::channel();
  let spawned = std::thread::Builder::new()
    .name("notification-thumbnail".into())
    .spawn(move || {
      let result = with_com(|| extract(&path, size));
      NOTIFICATION_BUSY.store(false, Ordering::Release);
      // The caller may have stopped waiting; its thumbnail is simply dropped.
      let _ = sender.send(result);
    });
  if let Err(error) = spawned {
    NOTIFICATION_BUSY.store(false, Ordering::Release);
    return Err(format!("Cannot start the thumbnail thread: {error}"));
  }
  match tokio::time::timeout(timeout, receiver).await {
    Ok(Ok(result)) => result,
    Ok(Err(_)) => Err("The thumbnail thread ended without a result".into()),
    Err(_) => Err(format!(
      "The Shell took longer than {} ms",
      timeout.as_millis()
    )),
  }
}

/// Run `work` with COM in a single-threaded apartment, as Shell thumbnail handlers expect.
fn with_com<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
  // SAFETY: initialized on the calling thread and balanced below on success.
  let com = unsafe {
    windows::Win32::System::Com::CoInitializeEx(
      None,
      windows::Win32::System::Com::COINIT_APARTMENTTHREADED
        | windows::Win32::System::Com::COINIT_DISABLE_OLE1DDE,
    )
  };
  if com.is_err() {
    return Err(format!("COM unavailable: {com:?}"));
  }
  let result = work();
  // SAFETY: balances the successful initialization above on the same thread.
  unsafe { windows::Win32::System::Com::CoUninitialize() };
  result
}

/// Extract thumbnails at most `size` physical pixels on their longer side, in order, delivering
/// each result from the worker thread. Returns false when no worker was started: one is already
/// running, or the thread could not be created.
pub(crate) fn request(
  paths: Vec<PathBuf>,
  size: u32,
  deliver: impl Fn(PathBuf, Result<Pixels, String>) + Send + 'static,
) -> bool {
  if paths.is_empty() {
    return false;
  }
  match WORKER.lock() {
    Ok(mut worker) => {
      if let Some(running) = worker.as_mut() {
        if running.started.elapsed() >= STALL && !running.reported {
          running.reported = true;
          tracing::warn!(
            elapsed_ms = running.started.elapsed().as_millis() as u64,
            "Recent capture thumbnails are not responding; showing type icons"
          );
        }
        return false;
      }
      *worker = Some(Worker {
        started: Instant::now(),
        reported: false,
      });
    },
    Err(error) => {
      tracing::error!(%error, "Thumbnail worker state poisoned");
      return false;
    },
  }
  let spawned = std::thread::Builder::new()
    .name("tray-thumbnails".into())
    .spawn(move || {
      let delivered = with_com(|| {
        for path in &paths {
          deliver(path.clone(), extract(path, size));
        }
        Ok(())
      });
      if let Err(error) = delivered {
        for path in paths {
          deliver(path, Err(error.clone()));
        }
      }
      finish();
    });
  match spawned {
    Ok(_) => true,
    Err(error) => {
      tracing::error!(%error, "Cannot start the thumbnail worker");
      finish();
      false
    },
  }
}

fn finish() {
  match WORKER.lock() {
    Ok(mut worker) => *worker = None,
    Err(error) => tracing::error!(%error, "Thumbnail worker state poisoned"),
  }
}

fn extract(path: &Path, size: u32) -> Result<Pixels, String> {
  use std::os::windows::ffi::OsStrExt;
  use windows::Win32::Foundation::SIZE;
  use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT, SIIGBF_THUMBNAILONLY,
  };
  use windows::core::PCWSTR;
  let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
  let side = size.clamp(16, 1024) as i32;
  // SAFETY: the path is NUL-terminated and outlives the call; the factory is released on drop.
  let factory: IShellItemImageFactory =
    unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }
      .map_err(|error| format!("Shell item: {error}"))?;
  // SAFETY: the returned bitmap is owned here and deleted by `read_bitmap`.
  let bitmap = unsafe {
    factory.GetImage(
      SIZE { cx: side, cy: side },
      SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY,
    )
  }
  .map_err(|error| format!("Shell thumbnail: {error}"))?;
  read_bitmap(bitmap)
}

/// Copy a top-down 32-bit image out of a Shell bitmap, then delete the bitmap.
fn read_bitmap(bitmap: windows::Win32::Graphics::Gdi::HBITMAP) -> Result<Pixels, String> {
  use windows::Win32::Graphics::Gdi::*;
  struct Owned(HBITMAP);
  impl Drop for Owned {
    fn drop(&mut self) {
      // SAFETY: the Shell handed ownership of this bitmap to the caller.
      if !unsafe { DeleteObject(self.0.into()) }.as_bool() {
        tracing::warn!("Cannot delete a thumbnail bitmap");
      }
    }
  }
  let bitmap = Owned(bitmap);
  let mut info = BITMAP::default();
  // SAFETY: info is a writable BITMAP of the size passed.
  let read = unsafe {
    GetObjectW(
      bitmap.0.into(),
      std::mem::size_of::<BITMAP>() as i32,
      Some((&mut info as *mut BITMAP).cast()),
    )
  };
  if read == 0 || info.bmWidth <= 0 || info.bmHeight == 0 {
    return Err("Shell thumbnail has no pixels".into());
  }
  let (width, height) = (info.bmWidth, info.bmHeight.abs());
  let mut header = BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER {
      biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: width,
      biHeight: -height,
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    },
    ..Default::default()
  };
  let mut pixels = vec![0u8; width as usize * height as usize * 4];
  // SAFETY: a memory DC compatible with the screen; the bitmap is not selected into any DC and
  // the buffer holds exactly `height` top-down rows of 32-bit pixels.
  let copied = unsafe {
    let dc = CreateCompatibleDC(None);
    if dc.is_invalid() {
      return Err("Cannot create a DC for the thumbnail".into());
    }
    let copied = GetDIBits(
      dc,
      bitmap.0,
      0,
      height as u32,
      Some(pixels.as_mut_ptr().cast()),
      &mut header,
      DIB_RGB_COLORS,
    );
    if !DeleteDC(dc).as_bool() {
      tracing::warn!("Cannot delete the thumbnail DC");
    }
    copied
  };
  if copied != height {
    return Err(format!("Copied {copied} of {height} thumbnail rows"));
  }
  bgra_to_rgba(&mut pixels);
  Ok(Pixels {
    width: width as u32,
    height: height as u32,
    rgba: pixels,
  })
}

/// Divide premultiplied color by alpha, in place.
fn unpremultiply(pixels: &mut [u8]) {
  for pixel in pixels.as_chunks_mut::<4>().0 {
    let alpha = u16::from(pixel[3]);
    if alpha != 0 && alpha != 255 {
      for channel in &mut pixel[..3] {
        *channel = ((u16::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
      }
    }
  }
}

/// Swap BGRA to RGBA in place. Shell bitmaps are premultiplied, except that images without an
/// alpha channel come back with every alpha byte zero; those are opaque.
fn bgra_to_rgba(pixels: &mut [u8]) {
  let opaque = pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0);
  for pixel in pixels.as_chunks_mut::<4>().0 {
    pixel.swap(0, 2);
    if opaque {
      pixel[3] = 255;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::mpsc;

  #[test]
  fn shell_pixels_become_rgba_and_alphaless_images_become_opaque() {
    let mut alphaless = vec![10, 20, 30, 0, 40, 50, 60, 0];
    bgra_to_rgba(&mut alphaless);
    assert_eq!(alphaless, [30, 20, 10, 255, 60, 50, 40, 255]);
    let mut translucent = vec![10, 20, 30, 128, 0, 0, 0, 0];
    bgra_to_rgba(&mut translucent);
    assert_eq!(
      translucent,
      [30, 20, 10, 128, 0, 0, 0, 0],
      "premultiplied alpha is kept, transparent pixels stay transparent"
    );
  }

  #[test]
  fn straight_alpha_restores_color_and_keeps_opaque_and_empty_pixels() {
    let pixels = Pixels {
      width: 3,
      height: 1,
      rgba: vec![64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0],
    }
    .into_straight();
    assert_eq!(pixels.rgba, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
  }

  #[test]
  fn notification_thumbnails_wait_briefly_one_at_a_time() {
    let folder = tempfile::tempdir().unwrap();
    let image = folder.path().join("capture.png");
    image::RgbaImage::from_pixel(40, 20, image::Rgba([0, 128, 255, 255]))
      .save(&image)
      .unwrap();
    let runtime = crate::app::runtime::test_runtime();
    let pixels = runtime
      .block_on(extract_bounded(image.clone(), 64, Duration::from_secs(20)))
      .unwrap()
      .into_straight();
    assert!(
      pixels.width <= 64 && pixels.width == pixels.height * 2,
      "{}x{}",
      pixels.width,
      pixels.height
    );
    assert_eq!(&pixels.rgba[..4], &[0, 128, 255, 255]);
    let missing = runtime.block_on(extract_bounded(
      folder.path().join("gone.mp4"),
      64,
      Duration::from_secs(20),
    ));
    assert!(missing.is_err());
    // A caller that gives up leaves the extraction running; the next call does not start another.
    NOTIFICATION_BUSY.store(true, Ordering::Release);
    let busy = runtime.block_on(extract_bounded(image, 64, Duration::from_secs(20)));
    NOTIFICATION_BUSY.store(false, Ordering::Release);
    assert!(busy.unwrap_err().contains("still being extracted"));
  }

  #[test]
  fn one_worker_extracts_in_order_reports_missing_files_and_frees_itself() {
    let folder = tempfile::tempdir().unwrap();
    let image = folder.path().join("capture.png");
    let mut png = image::RgbaImage::new(64, 32);
    for (x, _, pixel) in png.enumerate_pixels_mut() {
      *pixel = image::Rgba([if x < 32 { 255 } else { 0 }, 0, 255, 255]);
    }
    png.save(&image).unwrap();
    let missing = folder.path().join("gone.png");
    let (sender, receiver) = mpsc::channel();
    assert!(request(
      vec![image.clone(), missing.clone()],
      48,
      move |path, result| {
        let _ = sender.send((path, result));
      }
    ));
    assert!(
      !request(vec![image.clone()], 48, |_, _| {}),
      "a second worker never starts while one runs"
    );
    let (first, extracted) = receiver.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(first, image);
    let pixels = extracted.unwrap();
    assert!(pixels.width <= 48 && pixels.height <= 48);
    assert_eq!(pixels.width, pixels.height * 2, "the aspect ratio is kept");
    assert_eq!(
      pixels.rgba.len(),
      (pixels.width * pixels.height * 4) as usize
    );
    assert_eq!(
      &pixels.rgba[..4],
      &[255, 0, 255, 255],
      "red and blue are not swapped"
    );
    let (second, failed) = receiver.recv_timeout(Duration::from_secs(20)).unwrap();
    assert_eq!(second, missing);
    assert!(failed.is_err());
    let deadline = Instant::now() + Duration::from_secs(5);
    while WORKER.lock().unwrap().is_some() {
      assert!(Instant::now() < deadline, "the worker frees its slot");
      std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!request(Vec::new(), 48, |_, _| {}), "nothing to extract");
  }
}
