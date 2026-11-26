// Flinch-local Windows text smoothing patch. Upstream issue: slint-ui/slint#5748.
use skia_safe::{PixelGeometry, SurfaceProps, font::Edging};
use std::sync::OnceLock;
use windows::Win32::UI::WindowsAndMessaging::{
  FE_FONTSMOOTHINGCLEARTYPE, FE_FONTSMOOTHINGORIENTATIONBGR, FE_FONTSMOOTHINGORIENTATIONRGB,
  FE_FONTSMOOTHINGSTANDARD, SPI_GETFONTSMOOTHING, SPI_GETFONTSMOOTHINGORIENTATION,
  SPI_GETFONTSMOOTHINGTYPE, SYSTEM_PARAMETERS_INFO_ACTION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
  SystemParametersInfoW,
};

pub(crate) struct TextSettings {
  pub(crate) surface_props: SurfaceProps,
  pub(crate) edging: Edging,
}

fn classify(enabled: bool, kind: u32, orientation: u32) -> Result<TextSettings, String> {
  let (edging, geometry) = if !enabled {
    (Edging::Alias, PixelGeometry::Unknown)
  } else if kind == FE_FONTSMOOTHINGSTANDARD {
    (Edging::AntiAlias, PixelGeometry::Unknown)
  } else if kind == FE_FONTSMOOTHINGCLEARTYPE {
    let geometry = match orientation {
      FE_FONTSMOOTHINGORIENTATIONRGB => PixelGeometry::RGBH,
      FE_FONTSMOOTHINGORIENTATIONBGR => PixelGeometry::BGRH,
      other => {
        return Err(format!(
          "Unknown Windows font smoothing orientation: {other}"
        ));
      },
    };
    (Edging::SubpixelAntiAlias, geometry)
  } else {
    return Err(format!("Unknown Windows font smoothing type: {kind}"));
  };
  Ok(TextSettings {
    surface_props: SurfaceProps::new(Default::default(), geometry),
    edging,
  })
}

fn query(action: SYSTEM_PARAMETERS_INFO_ACTION) -> Result<u32, String> {
  let mut value = 0u32;
  // SAFETY: Each queried action writes one UINT or BOOL (both 32 bits) to this
  // live, aligned output. This reads preferences and never changes Windows settings.
  unsafe {
    SystemParametersInfoW(
      action,
      0,
      Some((&mut value as *mut u32).cast()),
      SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
  }
  .map_err(|error| format!("Windows font smoothing query {} failed: {error}", action.0))?;
  Ok(value)
}

fn read() -> Result<TextSettings, String> {
  let enabled = query(SPI_GETFONTSMOOTHING)? != 0;
  if !enabled {
    return classify(false, 0, 0);
  }
  let kind = query(SPI_GETFONTSMOOTHINGTYPE)?;
  let orientation = if kind == FE_FONTSMOOTHINGCLEARTYPE {
    query(SPI_GETFONTSMOOTHINGORIENTATION)?
  } else {
    0
  };
  classify(true, kind, orientation)
}

pub(crate) fn settings() -> &'static TextSettings {
  // Three bounded read-only Windows calls at most, once per process. There are
  // no channels, workers, callbacks or retries. Restart after changing ClearType.
  static SETTINGS: OnceLock<TextSettings> = OnceLock::new();
  SETTINGS.get_or_init(|| match read() {
    Ok(settings) => settings,
    Err(error) => {
      eprintln!("{error}; Slint will use grayscale text antialiasing.");
      TextSettings {
        surface_props: SurfaceProps::default(),
        edging: Edging::AntiAlias,
      }
    },
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn opaque_srgb_text_uses_lcd_channels_only_for_cleartype() {
    use skia_safe::{AlphaType, Color, ColorType, Font, FontMgr, FontStyle, Paint};
    let face = FontMgr::new()
      .match_family_style("Segoe UI", FontStyle::normal())
      .expect("supported Windows font");
    for scale in [1.0, 1.25, 1.5, 2.0] {
      for (kind, orientation, lcd) in [
        (FE_FONTSMOOTHINGSTANDARD, 0, false),
        (
          FE_FONTSMOOTHINGCLEARTYPE,
          FE_FONTSMOOTHINGORIENTATIONRGB,
          true,
        ),
        (
          FE_FONTSMOOTHINGCLEARTYPE,
          FE_FONTSMOOTHINGORIENTATIONBGR,
          true,
        ),
      ] {
        let settings = classify(true, kind, orientation).expect("known settings");
        let mut pixels = vec![0u8; 400 * 80 * 4];
        let info = crate::image_info((400, 80), ColorType::BGRA8888, AlphaType::Opaque);
        let mut surface = skia_safe::surfaces::wrap_pixels(
          &info,
          pixels.as_mut_slice(),
          None,
          Some(&settings.surface_props),
        )
        .expect("opaque production sRGB surface");
        surface.canvas().clear(Color::from_rgb(29, 29, 29));
        let mut font = Font::from_typeface(face.clone(), 14.0 * scale);
        font.set_subpixel(true).set_edging(settings.edging);
        let mut paint = Paint::default();
        paint.set_shader(crate::color_shader(&i_slint_core::Color::from_rgb_u8(
          241, 241, 241,
        )));
        surface
          .canvas()
          .draw_str("About & Help", (12.0, 40.0), &font, &paint);
        drop(surface);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
        let colored = pixels.chunks_exact(4).any(|p| p[0] != p[1] || p[1] != p[2]);
        assert_eq!(
          colored, lcd,
          "LCD channels at scale {scale}, orientation {orientation}"
        );
      }
    }
  }

  #[test]
  fn smoothing_modes_preserve_windows_preferences() {
    let off = classify(false, 99, 99).expect("disabled ignores LCD settings");
    assert_eq!(off.edging, Edging::Alias);
    assert_eq!(off.surface_props.pixel_geometry(), PixelGeometry::Unknown);
    let gray = classify(true, FE_FONTSMOOTHINGSTANDARD, 99).expect("standard smoothing");
    assert_eq!(gray.edging, Edging::AntiAlias);
    assert_eq!(gray.surface_props.pixel_geometry(), PixelGeometry::Unknown);
    for (orientation, geometry) in [
      (FE_FONTSMOOTHINGORIENTATIONRGB, PixelGeometry::RGBH),
      (FE_FONTSMOOTHINGORIENTATIONBGR, PixelGeometry::BGRH),
    ] {
      let lcd = classify(true, FE_FONTSMOOTHINGCLEARTYPE, orientation).expect("LCD smoothing");
      assert_eq!(lcd.edging, Edging::SubpixelAntiAlias);
      assert_eq!(lcd.surface_props.pixel_geometry(), geometry);
    }
    assert!(classify(true, 99, 0).is_err());
    assert!(classify(true, FE_FONTSMOOTHINGCLEARTYPE, 99).is_err());
  }
}
