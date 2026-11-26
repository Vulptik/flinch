//! Overlay-thread-owned composite cache. It contains no draft or editor chrome.

use anyhow::{Result, ensure};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{BitBlt, HDC, SRCCOPY};

use crate::overlay::gdi::layered::LayeredBitmap;

const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Key {
  pub crop: [u64; 4],
  pub region: [i32; 4],
  pub revision: u64,
  pub show_indicator: bool,
  pub excluded: Option<u64>,
}

impl Key {
  pub fn rect(self) -> RECT {
    RECT {
      left: self.region[0],
      top: self.region[1],
      right: self.region[2],
      bottom: self.region[3],
    }
  }

  fn bytes(self) -> u64 {
    area(self.rect()).saturating_mul(4)
  }
}

#[derive(Default)]
pub(super) struct SceneCache {
  key: Option<Key>,
  failed: Option<Key>,
  bitmap: Option<LayeredBitmap>,
}

impl SceneCache {
  pub fn bytes(&self) -> u64 {
    self
      .bitmap
      .as_ref()
      .map(|bitmap| bitmap.width as u64 * bitmap.height as u64 * 4)
      .unwrap_or_default()
  }

  pub fn restore(&mut self, key: Key, dc: HDC, damage: RECT) -> Result<Option<u64>> {
    if self.key != Some(key) {
      return Ok(None);
    }
    let Some(bitmap) = &self.bitmap else {
      return Ok(None);
    };
    let rect = intersect(key.rect(), damage);
    // Damage reaching past the cache, such as a drawing's handles dragged far outside the crop,
    // must rebuild the scene: a partial restore would leave the old handles there as a trail.
    if rect != damage {
      return Ok(None);
    }
    let pixels = area(rect);
    if pixels != 0 {
      // SAFETY: both DCs are live on the overlay thread; intersection stays in the cache surface.
      unsafe {
        BitBlt(
          dc,
          rect.left,
          rect.top,
          rect.right - rect.left,
          rect.bottom - rect.top,
          Some(bitmap.dc),
          rect.left - key.region[0],
          rect.top - key.region[1],
          SRCCOPY,
        )
      }?;
    }
    Ok(Some(pixels))
  }

  pub fn capture(&mut self, key: Key, dc: HDC) -> Result<()> {
    if self.failed == Some(key) {
      return Ok(());
    }
    self.key = None;
    let result = self.capture_inner(key, dc);
    if result.is_err() {
      self.failed = Some(key);
      self.bitmap = None;
    }
    result
  }

  fn capture_inner(&mut self, key: Key, dc: HDC) -> Result<()> {
    ensure!(
      key.bytes() != 0 && key.bytes() <= MAX_CACHE_BYTES,
      "Screenshot scene cache exceeds 64 MiB budget ({} bytes)",
      key.bytes()
    );
    let rect = key.rect();
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if self
      .bitmap
      .as_ref()
      .is_none_or(|bitmap| bitmap.width != width || bitmap.height != height)
    {
      // Release the previous surface before allocating its replacement; retained and transient
      // cache surfaces must both stay within the one-surface budget.
      self.bitmap = None;
      self.bitmap = Some(LayeredBitmap::new(width, height).map_err(anyhow::Error::msg)?);
    }
    if let Some(bitmap) = &self.bitmap {
      // SAFETY: bounded dimensions, live thread-owned DCs. Snapshot precedes dynamic chrome.
      unsafe {
        BitBlt(
          bitmap.dc,
          0,
          0,
          width,
          height,
          Some(dc),
          rect.left,
          rect.top,
          SRCCOPY,
        )
      }?;
      self.key = Some(key);
      self.failed = None;
    }
    Ok(())
  }

  pub fn invalidate(&mut self) {
    self.key = None;
  }
}

pub(super) fn intersect(a: RECT, b: RECT) -> RECT {
  RECT {
    left: a.left.max(b.left),
    top: a.top.max(b.top),
    right: a.right.min(b.right),
    bottom: a.bottom.min(b.bottom),
  }
}

pub(super) fn union(a: RECT, b: RECT) -> RECT {
  RECT {
    left: a.left.min(b.left),
    top: a.top.min(b.top),
    right: a.right.max(b.right),
    bottom: a.bottom.max(b.bottom),
  }
}

pub(super) fn area(rect: RECT) -> u64 {
  (rect.right - rect.left).max(0) as u64 * (rect.bottom - rect.top).max(0) as u64
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cache_rejects_oversized_surfaces_without_allocating_or_repeated_errors() {
    let key = Key {
      crop: [0; 4],
      region: [0, 0, 16384, 16384],
      revision: 0,
      show_indicator: true,
      excluded: None,
    };
    let mut cache = SceneCache::default();
    assert!(cache.capture(key, HDC::default()).is_err());
    assert_eq!(cache.bytes(), 0);
    assert!(
      cache.capture(key, HDC::default()).is_ok(),
      "failure is reported once per key, not on every pointer event"
    );
    assert!(
      cache
        .restore(key, HDC::default(), key.rect())
        .expect("miss")
        .is_none()
    );
  }
}
