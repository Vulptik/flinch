//! DirectWrite layouts shared by committed annotation measurement and painting.
//! Native EDIT input owns its caret/IME separately; no render callbacks or waits are added.

use anyhow::{Result, ensure};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::w;

use super::annotations::{TextAppearance, TextData};

pub(super) const MAX_HEIGHT: f32 = 262_144.0;

pub(super) fn format(
  factory: &IDWriteFactory,
  size: i32,
  centered: bool,
) -> Result<IDWriteTextFormat> {
  ensure!((12..=64).contains(&size), "Invalid annotation font size");
  // SAFETY: factory and formats remain on their caller thread; names are static UTF-16.
  unsafe {
    let format = factory.CreateTextFormat(
      w!("Segoe UI"),
      None,
      DWRITE_FONT_WEIGHT_NORMAL,
      DWRITE_FONT_STYLE_NORMAL,
      DWRITE_FONT_STRETCH_NORMAL,
      size as f32,
      w!(""),
    )?;
    format.SetTextAlignment(if centered {
      DWRITE_TEXT_ALIGNMENT_CENTER
    } else {
      DWRITE_TEXT_ALIGNMENT_LEADING
    })?;
    format.SetParagraphAlignment(if centered {
      DWRITE_PARAGRAPH_ALIGNMENT_CENTER
    } else {
      DWRITE_PARAGRAPH_ALIGNMENT_NEAR
    })?;
    format.SetWordWrapping(if centered {
      DWRITE_WORD_WRAPPING_NO_WRAP
    } else {
      DWRITE_WORD_WRAPPING_EMERGENCY_BREAK
    })?;
    Ok(format)
  }
}

pub(super) fn layout(
  factory: &IDWriteFactory,
  format: &IDWriteTextFormat,
  content: &str,
  width: i32,
  centered: bool,
) -> Result<IDWriteTextLayout> {
  ensure!(
    content.chars().count() <= 2048 && (1..=8192).contains(&width),
    "Invalid annotation text layout"
  );
  let wide: Vec<_> = content.encode_utf16().collect();
  // SAFETY: bounded UTF-16 input stays live for creation; the resulting layout owns its text.
  Ok(unsafe {
    factory.CreateTextLayout(
      &wide,
      format,
      width as f32,
      if centered { 36.0 } else { MAX_HEIGHT },
    )?
  })
}

pub(super) fn measure_text(content: String, size: i32, wrap_width: i32) -> Result<TextData> {
  // SAFETY: factory and all derived resources are used and released synchronously on this thread.
  let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
  let format = format(&factory, size, false)?;
  let layout = layout(&factory, &format, &content, wrap_width, false)?;
  let mut metrics = DWRITE_TEXT_METRICS::default();
  // SAFETY: owned layout and valid output; measurement uses exactly the painting layout.
  let overhang = unsafe {
    layout.GetMetrics(&mut metrics)?;
    layout.GetOverhangMetrics()?
  };
  let width = metrics.widthIncludingTrailingWhitespace + overhang.right.max(0.0);
  let height = metrics.height + overhang.bottom.max(0.0);
  ensure!(
    width.is_finite() && height.is_finite() && height <= MAX_HEIGHT,
    "Annotation text exceeds layout budget"
  );
  Ok(TextData {
    content,
    size,
    wrap_width,
    extent: (width.ceil().max(1.0) as i32, height.ceil().max(1.0) as i32),
    appearance: TextAppearance::Plain,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn measurement_handles_fallback_scripts_wrapping_and_limits() {
    for content in ["Café 日本語 🙂", "مرحبا بالعالم", "a\n\nb", "", "e\u{301}"] {
      let data = measure_text(content.into(), 24, 120).expect("DirectWrite measurement");
      assert!(data.extent.0 > 0 && data.extent.1 > 0);
      let repeated = measure_text(content.into(), 24, 120).expect("repeat measurement");
      assert_eq!(data, repeated);
    }
    let narrow = measure_text("unbrokenword".repeat(10), 24, 40).expect("emergency wrap");
    let wide = measure_text("unbrokenword".repeat(10), 24, 400).expect("wide wrap");
    assert!(narrow.extent.1 > wide.extent.1);
    assert!(measure_text("x".repeat(2049), 24, 120).is_err());
    assert!(measure_text("x".into(), 65, 120).is_err());
    assert!(measure_text("x".into(), 24, 0).is_err());
  }
}
