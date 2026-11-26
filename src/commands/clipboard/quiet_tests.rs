use super::*;
use std::cell::RefCell;

#[test]
fn clipboard_retry_has_exact_attempt_budget_backoff_and_original_failure() {
  for success in 1..=5 {
    let attempts = RefCell::new(Vec::new());
    let delays = RefCell::new(Vec::new());
    let (value, _, count) = run_clipboard_retry_with_delay(
      "test",
      |n| {
        attempts.borrow_mut().push(n);
        if n == success {
          Ok("copied")
        } else {
          Err(format!("busy {n}"))
        }
      },
      |d| delays.borrow_mut().push(d.as_millis()),
    )
    .expect("eventual success");
    assert_eq!((value, count), ("copied", success));
    assert_eq!(*attempts.borrow(), (1..=success).collect::<Vec<_>>());
    assert_eq!(
      *delays.borrow(),
      (1..success)
        .map(|n| u128::from(n) * 300)
        .collect::<Vec<_>>()
    );
  }
  let mut attempts = 0;
  let error = run_clipboard_retry_with_delay::<()>(
    "test",
    |n| {
      attempts += 1;
      Err(format!("native failure {n}"))
    },
    |_| {},
  )
  .unwrap_err();
  assert_eq!(attempts, 5);
  assert_eq!(error, "native failure 5 (after 5 attempts)");
}

#[test]
fn clipboard_dib_serialization_preserves_rows_alpha_and_bgra_order() {
  let rgba = [
    10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, 130, 140, 150, 160,
  ];
  let memory = OwnedGlobal(alloc_dib_handle(2, 2, &rgba).expect("DIB allocation"));
  // SAFETY: initialized movable allocation is locked only for these bounded reads.
  unsafe {
    let p = GlobalLock(memory.0);
    assert!(!p.is_null());
    let header = &*p.cast::<BITMAPINFOHEADER>();
    assert_eq!(
      (
        header.biWidth,
        header.biHeight,
        header.biPlanes,
        header.biBitCount,
        header.biSizeImage
      ),
      (2, -2, 1, 32, 16)
    );
    let data = std::slice::from_raw_parts(p.cast::<u8>().add(header.biSize as usize), 16);
    assert_eq!(
      data,
      &[
        30, 20, 10, 40, 70, 60, 50, 80, 110, 100, 90, 120, 150, 140, 130, 160
      ]
    );
    let _ = GlobalUnlock(memory.0);
  }
  for (w, h, data) in [
    (0, 1, &[][..]),
    (1, 0, &[][..]),
    (usize::MAX, 2, &[][..]),
    (1, 1, &[0u8; 3][..]),
    (65536, 65536, &[][..]),
  ] {
    assert!(alloc_dib_handle(w, h, data).is_err());
  }
}

#[test]
fn clipboard_file_payload_is_unicode_double_terminated_and_rejects_nul() {
  use std::os::windows::ffi::OsStrExt;
  let path = Path::new(r"C:\captures\前_🌍.png");
  let memory = OwnedGlobal(alloc_hdrop_handle(path).expect("file allocation"));
  // SAFETY: header and UTF-16 payload were allocated and initialized above.
  unsafe {
    let p = GlobalLock(memory.0);
    assert!(!p.is_null());
    let header = &*p.cast::<DROPFILES>();
    assert!(header.fWide.as_bool());
    assert!(!header.fNC.as_bool());
    assert_eq!(header.pFiles as usize, std::mem::size_of::<DROPFILES>());
    let expected = path
      .as_os_str()
      .encode_wide()
      .chain([0, 0])
      .collect::<Vec<_>>();
    assert_eq!(
      std::slice::from_raw_parts(
        p.cast::<u8>().add(header.pFiles as usize).cast::<u16>(),
        expected.len()
      ),
      expected
    );
    let _ = GlobalUnlock(memory.0);
  }
  assert!(alloc_hdrop_handle(Path::new("")).is_err());
  assert!(alloc_hdrop_handle(Path::new("capture\0hidden.png")).is_err());
}

struct FakeClipboard {
  fail: &'static str,
  events: RefCell<Vec<String>>,
  // A successful publication transfers ownership to this fake service.
  adopted: RefCell<Vec<OwnedGlobal>>,
}
impl ClipboardApi for FakeClipboard {
  fn open(&self) -> Result<(), String> {
    self.call("open")
  }
  fn empty(&self) -> Result<(), String> {
    self.call("empty")
  }
  fn close(&self) -> Result<(), String> {
    self.call("close")
  }
  fn set(&self, format: u32, handle: HGLOBAL) -> Result<(), String> {
    self.call(if format == CF_DIB_FORMAT {
      "image"
    } else {
      "file"
    })?;
    self.adopted.borrow_mut().push(OwnedGlobal(handle));
    Ok(())
  }
}
impl FakeClipboard {
  fn call(&self, operation: &str) -> Result<(), String> {
    self.events.borrow_mut().push(operation.into());
    if self.fail.split(',').any(|stage| stage == operation) {
      Err(format!("{operation} failed"))
    } else {
      Ok(())
    }
  }
}

#[test]
fn clipboard_transaction_closes_on_every_failure_and_transfers_only_accepted_handles() {
  for (fail, events, adopted) in [
    ("open", vec!["open"], 0),
    ("empty", vec!["open", "empty", "close"], 0),
    ("image", vec!["open", "empty", "image", "close"], 0),
    ("file", vec!["open", "empty", "image", "file", "close"], 1),
    ("close", vec!["open", "empty", "image", "file", "close"], 2),
    ("", vec!["open", "empty", "image", "file", "close"], 2),
    ("image,close", vec!["open", "empty", "image", "close"], 0),
  ] {
    let api = FakeClipboard {
      fail,
      events: RefCell::default(),
      adopted: RefCell::default(),
    };
    let image = OwnedGlobal(alloc_dib_handle(1, 1, &[1, 2, 3, 4]).unwrap());
    let file = OwnedGlobal(alloc_hdrop_handle(Path::new(r"C:\capture.png")).unwrap());
    let result = publish_image_file(image, file, &api);
    assert_eq!(result.is_ok(), fail.is_empty());
    assert_eq!(*api.events.borrow(), events);
    assert_eq!(api.adopted.borrow().len(), adopted);
    if fail == "image,close" {
      let error = result.unwrap_err();
      assert!(error.contains("image failed") && error.contains("close failed"));
    }
  }
}

#[tokio::test]
async fn clipboard_unavailable_is_explicit_without_touching_system_clipboard() {
  let app = AppHandle::isolated(crate::config::Config::default(), std::path::PathBuf::new());
  let result = copy_rgba_and_file_to_clipboard_with_retry(
    &app,
    1,
    1,
    std::sync::Arc::new(vec![1, 2, 3, 4]),
    "capture.png".into(),
  )
  .await;
  assert_eq!(result.unwrap_err(), "Clipboard is unavailable");
}

#[test]
fn file_only_copy_closes_on_every_failure_and_transfers_the_accepted_drop_list() {
  for (fail, events, adopted) in [
    ("open", vec!["open"], 0),
    ("empty", vec!["open", "empty", "close"], 0),
    ("file", vec!["open", "empty", "file", "close"], 0),
    ("close", vec!["open", "empty", "file", "close"], 1),
    ("", vec!["open", "empty", "file", "close"], 1),
  ] {
    let api = FakeClipboard {
      fail,
      events: RefCell::default(),
      adopted: RefCell::default(),
    };
    let file = OwnedGlobal(alloc_hdrop_handle(Path::new(r"C:\clip.mp4")).unwrap());
    assert_eq!(publish_file(file, &api).is_ok(), fail.is_empty());
    assert_eq!(*api.events.borrow(), events);
    assert_eq!(api.adopted.borrow().len(), adopted);
  }
}

#[tokio::test]
async fn file_copy_reports_an_unavailable_clipboard_without_touching_it() {
  let app = AppHandle::isolated(crate::config::Config::default(), std::path::PathBuf::new());
  let result = copy_file_to_clipboard_with_retry(&app, "clip.gif".into()).await;
  assert_eq!(result.unwrap_err(), "Clipboard is unavailable");
}

#[test]
fn screenshots_beyond_the_image_limit_copy_as_files() {
  assert!(fits_clipboard_image(1920, 1080));
  assert!(fits_clipboard_image(11_520, 2_160), "three 4K screens");
  assert!(fits_clipboard_image(8_192, 8_192), "exactly 256 MiB");
  assert!(!fits_clipboard_image(8_192, 8_193));
  assert!(!fits_clipboard_image(u32::MAX, u32::MAX), "no overflow");
}

#[tokio::test]
async fn image_copy_falls_back_to_the_file_and_large_ones_skip_the_image() {
  for (fits, image, file, expected, calls) in [
    (true, Ok(()), Ok(()), Ok(()), vec!["image"]),
    (
      true,
      Err("no memory"),
      Ok(()),
      Ok(()),
      vec!["image", "file"],
    ),
    (
      true,
      Err("no memory"),
      Err("busy"),
      Err("busy"),
      vec!["image", "file"],
    ),
    (false, Ok(()), Ok(()), Ok(()), vec!["file"]),
    (false, Ok(()), Err("busy"), Err("busy"), vec!["file"]),
  ] {
    let seen = RefCell::new(Vec::new());
    let result = image_or_file(
      fits,
      || {
        seen.borrow_mut().push("image");
        std::future::ready(image.map_err(str::to_owned))
      },
      || {
        seen.borrow_mut().push("file");
        std::future::ready(file.map_err(str::to_owned))
      },
    )
    .await;
    assert_eq!(result, expected.map_err(str::to_owned));
    assert_eq!(*seen.borrow(), calls);
  }
}

/// Too large an image is refused before any work; without a clipboard service nothing reaches
/// the system clipboard.
#[tokio::test]
async fn an_oversized_or_clipboardless_pixel_copy_fails_without_touching_the_clipboard() {
  let app = crate::app::context::AppHandle::isolated(
    crate::config::Config::default(),
    std::path::PathBuf::new(),
  );
  let pixels = std::sync::Arc::new(vec![0u8; 4]);
  let error = copy_rgba_to_clipboard_with_retry(&app, 20_000, 20_000, pixels.clone())
    .await
    .expect_err("too large");
  assert!(error.contains("too large"), "{error}");
  let error = copy_rgba_to_clipboard_with_retry(&app, 1, 1, pixels)
    .await
    .expect_err("no clipboard");
  assert!(error.contains("Clipboard is unavailable"), "{error}");
}
