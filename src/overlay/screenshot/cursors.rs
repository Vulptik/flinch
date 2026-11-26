//! Thread-owned annotation cursors.
//!
//! Drawing tools borrow Windows cursors, so they follow the user's pointer settings. Moving
//! and rotating drawings, and the toolbars' drag grips, use Chromium's open and closed hands
//! from `resources/cursors/`, embedded unmodified and scaled to the system cursor size, as
//! Chrome does on Windows.
use anyhow::{Context, Result, ensure};
use windows::Win32::UI::WindowsAndMessaging::{
  CreateIconFromResourceEx, DestroyCursor, GetCursor, GetSystemMetrics, HCURSOR, IDC_ARROW,
  LR_DEFAULTCOLOR, LoadCursorW, SM_CXCURSOR, SetCursor,
};

const OPEN_HAND: &[u8] = include_bytes!("../../../resources/cursors/hand_grab.cur");
const CLOSED_HAND: &[u8] = include_bytes!("../../../resources/cursors/hand_grabbing.cur");

struct CursorImage<'a> {
  size: i32,
  bit_count: u16,
  hotspot: [u16; 2],
  bitmap: &'a [u8],
}
fn word(bytes: &[u8], offset: usize) -> Result<u16> {
  let bytes = bytes
    .get(offset..offset + 2)
    .context("Truncated cursor word")?;
  Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}
fn dword(bytes: &[u8], offset: usize) -> Result<u32> {
  let bytes = bytes
    .get(offset..offset + 4)
    .context("Truncated cursor offset")?;
  Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
/// Picks the image closest to `requested`, preferring more colors at equal sizes.
fn image_for_size(asset: &[u8], requested: i32) -> Result<CursorImage<'_>> {
  ensure!(
    word(asset, 0)? == 0 && word(asset, 2)? == 2,
    "Invalid CUR header"
  );
  let count = usize::from(word(asset, 4)?);
  ensure!(
    (1..=8).contains(&count) && requested > 0,
    "Invalid cursor image count or requested size"
  );
  let mut best: Option<CursorImage<'_>> = None;
  for index in 0..count {
    let entry = 6 + index * 16;
    let width = i32::from(*asset.get(entry).context("Truncated cursor directory")?);
    let height = i32::from(*asset.get(entry + 1).context("Truncated cursor directory")?);
    ensure!(
      (1..=128).contains(&width) && width == height,
      "Invalid cursor dimensions"
    );
    let hotspot = [word(asset, entry + 4)?, word(asset, entry + 6)?];
    ensure!(
      hotspot.iter().all(|&p| i32::from(p) < width),
      "Cursor hotspot outside image"
    );
    let offset = dword(asset, entry + 12)? as usize;
    let length = dword(asset, entry + 8)? as usize;
    ensure!(offset >= 6 + count * 16, "Cursor bitmap overlaps directory");
    let end = offset
      .checked_add(length)
      .context("Cursor bitmap offset overflow")?;
    let bitmap = asset.get(offset..end).context("Truncated cursor bitmap")?;
    let bit_count = word(bitmap, 14)?;
    ensure!(
      matches!(bit_count, 1 | 4 | 8 | 24 | 32),
      "Unsupported cursor color depth"
    );
    let colors = dword(bitmap, 32)? as usize;
    let palette = if bit_count <= 8 {
      4 * if colors == 0 { 1 << bit_count } else { colors }
    } else {
      0
    };
    let side = width as usize;
    let color_bytes = (side * usize::from(bit_count)).div_ceil(32) * 4 * side;
    let mask_bytes = side.div_ceil(32) * 4 * side;
    ensure!(
      bitmap.len() == 40 + palette + color_bytes + mask_bytes
        && dword(bitmap, 0)? == 40
        && dword(bitmap, 4)? == width as u32
        && dword(bitmap, 8)? == height as u32 * 2
        && word(bitmap, 12)? == 1
        && dword(bitmap, 16)? == 0,
      "Invalid cursor bitmap"
    );
    let better = best.as_ref().is_none_or(|previous| {
      let distance = (width - requested).abs();
      let previous_distance = (previous.size - requested).abs();
      distance < previous_distance
        || distance == previous_distance && bit_count > previous.bit_count
    });
    if better {
      best = Some(CursorImage {
        size: width,
        bit_count,
        hotspot,
        bitmap,
      });
    }
  }
  best.context("Cursor has no images")
}
/// Creates an owned cursor drawn at `size` pixels; Windows scales the image and hotspot.
fn load_cursor(asset: &[u8], size: i32) -> Result<HCURSOR> {
  let image = image_for_size(asset, size)?;
  // RT_CURSOR data prefixes the DIB with the hotspot, unlike the on-disk CUR directory.
  let mut resource = Vec::with_capacity(image.bitmap.len() + 4);
  for coordinate in image.hotspot {
    resource.extend_from_slice(&coordinate.to_le_bytes());
  }
  resource.extend_from_slice(image.bitmap);
  // SAFETY: the complete bounded DIB and hotspot were validated above. Windows copies bytes.
  // ficon=false creates an owned cursor; HICON and HCURSOR wrap the same underlying handle type.
  let cursor = unsafe {
    CreateIconFromResourceEx(&resource, false, 0x0003_0000, size, size, LR_DEFAULTCOLOR)
  }?;
  Ok(HCURSOR(cursor.0))
}

/// A borrowed Windows cursor; Windows owns its lifetime.
pub fn system(id: windows::core::PCWSTR) -> Result<HCURSOR> {
  // SAFETY: loads a shared system cursor without changing the active cursor.
  unsafe { LoadCursorW(None, id) }.context("Load Windows cursor")
}

fn release(cursor: HCURSOR) {
  // SAFETY: never destroy the active cursor; hand the pointer to the system arrow first.
  if unsafe { GetCursor() } == cursor {
    match system(IDC_ARROW) {
      Ok(arrow) => unsafe {
        SetCursor(Some(arrow));
      },
      Err(error) => tracing::warn!(%error, "Failed to restore system cursor during cleanup"),
    }
  }
  // SAFETY: this owner created the cursor with ficon=false on this thread.
  if let Err(error) = unsafe { DestroyCursor(cursor) } {
    tracing::warn!(%error, "Failed to release annotation cursor");
  }
}

pub struct ToolCursors {
  open: HCURSOR,
  closed: HCURSOR,
}
impl ToolCursors {
  pub fn new() -> Result<Self> {
    // SAFETY: read-only system cursor dimensions; no input or window changes.
    let size = unsafe { GetSystemMetrics(SM_CXCURSOR) };
    ensure!(size > 0, "Windows cursor size is unavailable");
    let open = load_cursor(OPEN_HAND, size).context("Failed to load open hand cursor")?;
    let closed = match load_cursor(CLOSED_HAND, size) {
      Ok(closed) => closed,
      Err(error) => {
        release(open);
        return Err(error.context("Failed to load closed hand cursor"));
      },
    };
    Ok(Self { open, closed })
  }
  /// Over a drawing that can be moved or rotated, or a toolbar's drag grip.
  pub fn open_hand(&self) -> HCURSOR {
    self.open
  }
  /// While a drawing is being moved or rotated, or a toolbar is being dragged.
  pub fn closed_hand(&self) -> HCURSOR {
    self.closed
  }
}
impl Drop for ToolCursors {
  fn drop(&mut self) {
    release(self.open);
    release(self.closed);
  }
}
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn chromium_hands_prefer_their_color_image_and_keep_the_palm_hotspot() {
    for asset in [OPEN_HAND, CLOSED_HAND] {
      for requested in [32, 48, 64] {
        let image = image_for_size(asset, requested).expect("cursor asset");
        assert_eq!(image.size, 32, "Chromium ships one 32px size");
        assert_eq!(
          image.bit_count, 24,
          "the color image wins over the mono one"
        );
        assert_eq!(image.hotspot, [13, 13]);
      }
    }
  }

  #[test]
  fn malformed_cursor_files_fail_explicitly() {
    assert!(image_for_size(&[], 32).is_err());
    assert!(image_for_size(OPEN_HAND, 0).is_err());
    let mut wrong_length = OPEN_HAND.to_vec();
    wrong_length[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(image_for_size(&wrong_length, 32).is_err());
    let mut hotspot = OPEN_HAND.to_vec();
    hotspot[10] = 40;
    assert!(image_for_size(&hotspot, 32).is_err());
    let mut depth = OPEN_HAND.to_vec();
    let offset = dword(&depth, 18).expect("first image offset") as usize;
    depth[offset + 14] = 16;
    assert!(image_for_size(&depth, 32).is_err());
  }

  #[test]
  fn native_hands_scale_with_the_system_cursor_and_draw_with_transparency() {
    use crate::overlay::gdi::bitmap::ScreenshotBitmap;
    use windows::Win32::Graphics::Gdi::DeleteObject;
    use windows::Win32::UI::WindowsAndMessaging::{
      DI_NORMAL, DrawIconEx, GetIconInfo, HICON, ICONINFO,
    };
    let cursors = ToolCursors::new().expect("native cursors");
    assert_ne!(cursors.open_hand(), cursors.closed_hand());
    // SAFETY: read-only system metric.
    let size = unsafe { GetSystemMetrics(SM_CXCURSOR) };
    let (width, height) = (256usize, 300usize);
    let mut source = vec![255u8; width * height * 4];
    for y in 0..height {
      for x in 0..width {
        let color = match y / 100 {
          0 => [240, 240, 240],
          1 => [28, 32, 40],
          _ if (x / 12 + y / 12) % 2 == 0 => [105, 130, 145],
          _ => [155, 135, 115],
        };
        source[(y * width + x) * 4..(y * width + x) * 4 + 3].copy_from_slice(&color);
      }
    }
    let mut bitmap =
      ScreenshotBitmap::from_rgba(&source, width as u32, height as u32).expect("canvas");
    for (index, cursor) in [cursors.open_hand(), cursors.closed_hand()]
      .into_iter()
      .enumerate()
    {
      let mut info = ICONINFO::default();
      // SAFETY: owned cursor and live local ICONINFO; returned bitmap copies are freed below.
      unsafe { GetIconInfo(HICON(cursor.0), &mut info) }.expect("cursor info");
      assert!(!info.fIcon.as_bool());
      let expected = (13 * size + 16) / 32;
      assert!(
        info.xHotspot.abs_diff(expected as u32) <= 1 && info.xHotspot == info.yHotspot,
        "palm hotspot scales with the cursor: {} for {size}px",
        info.xHotspot
      );
      for row in 0..3 {
        // SAFETY: this test owns the offscreen DC and cursor; no screen/input is accessed.
        unsafe {
          DrawIconEx(
            bitmap.dc,
            (index * 128 + 16) as i32,
            row * 100 + 16,
            HICON(cursor.0),
            size,
            size,
            0,
            None,
            DI_NORMAL,
          )
        }
        .expect("draw cursor");
      }
      // SAFETY: GetIconInfo transferred bitmap copies to this test.
      unsafe {
        assert!(DeleteObject(info.hbmMask.into()).as_bool());
        if !info.hbmColor.is_invalid() {
          assert!(DeleteObject(info.hbmColor.into()).as_bool());
        }
      }
    }
    let pixels = bitmap.to_rgba().expect("cursor pixels");
    assert_ne!(pixels, source, "both hands draw");
    for index in 0..2usize {
      for row in 0..3usize {
        let corner = ((row * 100 + 16) * width + index * 128 + 16) * 4;
        assert_eq!(
          &pixels[corner..corner + 3],
          &source[corner..corner + 3],
          "transparent corner"
        );
      }
    }
    if let Ok(folder) = std::env::var("FLINCH_ANNOTATION_PREVIEW_DIR") {
      std::fs::create_dir_all(&folder).expect("preview directory");
      image::save_buffer(
        std::path::Path::new(&folder).join("native-cursor-gallery.png"),
        &pixels,
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
      )
      .expect("native gallery");
    }
  }
}
