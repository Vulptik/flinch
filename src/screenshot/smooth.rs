//! Shared annotation painting on the existing screenshot DC.
//! Direct2D/DirectWrite resources stay within a bounded synchronous batch.

use super::annotations::{Annotation, Tool};
use anyhow::{Result, ensure};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetClipBox, HDC};

pub(super) fn draw<'a>(
  dc: HDC,
  mut annotations: impl Iterator<Item = &'a Annotation>,
  origin: (i32, i32),
  clip: RECT,
) -> Result<()> {
  let Some(first) = annotations.next() else {
    return Ok(());
  };
  let mut dc_clip = RECT::default();
  // SAFETY: caller owns the live DC and valid output storage.
  ensure!(
    unsafe { GetClipBox(dc, &mut dc_clip) }.0 != 0,
    "Failed to query annotation damage clip"
  );
  let clip = RECT {
    left: clip.left.max(dc_clip.left),
    top: clip.top.max(dc_clip.top),
    right: clip.right.min(dc_clip.right),
    bottom: clip.bottom.min(dc_clip.bottom),
  };
  if clip.right <= clip.left || clip.bottom <= clip.top {
    return Ok(());
  }
  let mut renderer = None;
  for (index, annotation) in std::iter::once(first).chain(annotations).enumerate() {
    ensure!(
      index < super::annotations::MAX_ANNOTATIONS,
      "Annotation count budget exceeded"
    );
    if annotation.tool == Tool::Select || annotation.points.is_empty() {
      continue;
    }
    if renderer.is_none() {
      renderer = Some(super::vector::Renderer::new()?);
    }
    renderer
      .as_mut()
      .ok_or_else(|| anyhow::anyhow!("Annotation renderer unavailable"))?
      .draw(dc, annotation, origin, clip)?;
  }
  Ok(())
}
