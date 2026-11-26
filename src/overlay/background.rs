//! One immutable dimmed desktop per selection thread. Never modifies capture pixels.

use windows::Win32::Graphics::Gdi::{BitBlt, GdiFlush, HDC, SRCCOPY};

use crate::overlay::gdi::layered::LayeredBitmap;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const DIM_ALPHA: u8 = 160;

/// Blend toward black on the CPU. A full-screen GDI `AlphaBlend` took about 13 ms at
/// 2560×1440, on the path to the first overlay frame.
fn dim_pixels(pixels: &mut [u8], alpha: u8) {
  let keep = 255 - u32::from(alpha);
  for value in pixels {
    *value = (((u32::from(*value) * keep + 128) * 257) >> 16) as u8;
  }
}

#[derive(Default)]
pub(crate) struct Background {
  bitmap: Option<LayeredBitmap>,
  attempted: bool,
}

impl Background {
  pub fn restore(&mut self, source: HDC, destination: HDC, width: i32, height: i32) -> bool {
    if !self.attempted {
      self.attempted = true;
      match self.build(source, width, height) {
        Ok(bitmap) => self.bitmap = Some(bitmap),
        Err(error) => {
          tracing::warn!(%error, "Dimmed background cache unavailable; using full renderer for this overlay")
        },
      }
    }
    let Some(bitmap) = &self.bitmap else {
      return false;
    };
    // SAFETY: immutable cache and destination DC are owned by this selection thread.
    match unsafe {
      BitBlt(
        destination,
        0,
        0,
        width,
        height,
        Some(bitmap.dc),
        0,
        0,
        SRCCOPY,
      )
    } {
      Ok(()) => true,
      Err(error) => {
        tracing::warn!(%error, "Dimmed background restore failed; disabling cache for this overlay");
        self.bitmap = None;
        false
      },
    }
  }

  fn build(&self, source: HDC, width: i32, height: i32) -> Result<LayeredBitmap, String> {
    if width <= 0 || height <= 0 || width as u64 * height as u64 * 4 > MAX_BYTES {
      return Err("Dimmed background exceeds 64 MiB budget".into());
    }
    let mut bitmap = LayeredBitmap::new(width, height)?;
    // SAFETY: live thread-owned DCs with equal dimensions; original capture remains untouched.
    unsafe {
      BitBlt(bitmap.dc, 0, 0, width, height, Some(source), 0, 0, SRCCOPY).map_err(|error| {
        crate::error::rich_display_context("Cache screenshot copy failed", error)
      })?;
      if !GdiFlush().as_bool() {
        return Err("Cache screenshot copy did not complete".into());
      }
    }
    dim_pixels(bitmap.pixels_mut(), DIM_ALPHA);
    Ok(bitmap)
  }

  #[cfg(test)]
  pub fn disable(&mut self) {
    self.attempted = true;
    self.bitmap = None;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::overlay::gdi::drawing::draw_dim_layer;

  #[test]
  fn cpu_dim_matches_gdi_for_every_channel_value() {
    let mut reference = LayeredBitmap::new(256, 1).expect("reference");
    for (level, pixel) in reference
      .pixels_mut()
      .as_chunks_mut::<4>()
      .0
      .iter_mut()
      .enumerate()
    {
      pixel.fill(level as u8);
    }
    let mut dimmed = reference.pixels_mut().to_vec();
    dim_pixels(&mut dimmed, DIM_ALPHA);
    // SAFETY: test owns the DC and flushes pending operations before DIB readback.
    unsafe {
      draw_dim_layer(reference.dc, 256, 1, DIM_ALPHA).expect("dim");
      assert!(GdiFlush().as_bool());
    }
    for (level, (cpu, gdi)) in dimmed
      .as_chunks::<4>()
      .0
      .iter()
      .zip(reference.pixels_mut().as_chunks::<4>().0)
      .enumerate()
    {
      assert_eq!(&cpu[..3], &gdi[..3], "level {level}");
    }
  }

  #[test]
  fn cache_matches_reference_and_preserves_source_across_restores() {
    let mut source = LayeredBitmap::new(32, 24).expect("source");
    source.pixels_mut().fill(120);
    let original = source.pixels_mut().to_vec();
    let mut reference = LayeredBitmap::new(32, 24).expect("reference");
    let mut cached = LayeredBitmap::new(32, 24).expect("cached");
    // SAFETY: test owns the DCs and flushes pending operations before DIB readback.
    unsafe {
      BitBlt(reference.dc, 0, 0, 32, 24, Some(source.dc), 0, 0, SRCCOPY).expect("copy");
      draw_dim_layer(reference.dc, 32, 24, 160).expect("dim");
      assert!(GdiFlush().as_bool());
    }
    let mut background = Background::default();
    for _ in 0..3 {
      assert!(background.restore(source.dc, cached.dc, 32, 24));
      // SAFETY: complete GDI writes before reading owned DIB pixel buffers.
      assert!(unsafe { GdiFlush() }.as_bool());
      // GDI does not promise preservation of alpha; compare exported RGB only.
      for (a, b) in cached
        .pixels_mut()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(reference.pixels_mut().as_chunks::<4>().0)
      {
        assert_eq!(&a[..3], &b[..3]);
      }
      assert_eq!(source.pixels_mut(), original);
    }
    let mut oversized = Background::default();
    assert!(!oversized.restore(source.dc, cached.dc, 32768, 32768));
    assert!(oversized.attempted);
    assert!(oversized.bitmap.is_none());
  }

  #[test]
  #[ignore = "explicit serial Windows selection drawing comparison"]
  fn selection_rendering_comparison() {
    use crate::overlay::gdi::bitmap::ScreenshotBitmap;
    use crate::overlay::selection::{Rect, SelectionInteraction};
    use crate::overlay::selection_core::{
      SelectionDrawColors, SharedSelectionOverlayState, draw_selection_scene,
    };
    use std::sync::Arc;
    use std::time::Instant;
    use windows::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject, HFONT};

    for (width, height) in [(2560u32, 1440u32), (3840, 2160)] {
      for cached in [false, true] {
        let source = Arc::new(vec![120; width as usize * height as usize * 4]);
        let mut shared =
          SharedSelectionOverlayState::new(width, height, source.clone(), 0, 0, Instant::now());
        shared.resources.screenshot =
          Some(ScreenshotBitmap::from_rgba(&source, width, height).expect("source"));
        let offscreen = ScreenshotBitmap::from_rgba(&source, width, height).expect("offscreen");
        shared.resources.offscreen_dc = offscreen.dc;
        if !cached {
          shared.resources.background.disable();
        }
        // SAFETY: stock font is borrowed, never deleted; DCs remain owned by this test.
        let font = unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) };
        let mut times = Vec::new();
        for frame in 0..65 {
          shared.controller.interaction = SelectionInteraction::Confirmed {
            rect: Rect {
              x: 100.0 + frame as f64,
              y: 80.0,
              w: 900.0,
              h: 700.0,
            },
            from_snap: false,
            moving: None,
            resizing: None,
          };
          let started = Instant::now();
          draw_selection_scene(&mut shared, font, SelectionDrawColors::DEFAULT);
          // SAFETY: finish native rasterization before timing the complete scene cost.
          assert!(unsafe { GdiFlush() }.as_bool());
          if frame != 0 {
            times.push(started.elapsed().as_micros() as u64);
          }
        }
        times.sort_unstable();
        println!(
          "{width}x{height} cached={cached} p50_us={} p95_us={}",
          times[31], times[60]
        );
      }
    }
  }
}
