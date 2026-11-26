//! Optional asset maintenance. Normal builds keep the reviewed raster assets untouched.
use crate::{json, read};
use anyhow::{Context, Result, ensure};
use image::{Rgba, RgbaImage, imageops};
use resvg::{tiny_skia, usvg};
use sha1::{Digest, Sha1};
use std::{
  collections::BTreeMap,
  fs,
  io::Cursor,
  path::{Path, PathBuf},
};
const SOURCE: &str = "icons/source";
/// The transparent mark is the settings logo itself; there is no separate copy.
const LOGO_ONLY: &str = "crates/settings/ui/assets/logo.svg";
const PROVENANCE: &str = "resources/branding-provenance.json";

fn raster(svg: &[u8], width: u32, height: u32) -> Result<RgbaImage> {
  let mut options = usvg::Options::default();
  options.fontdb_mut().load_system_fonts();
  let tree = usvg::Tree::from_data(svg, &options)?;
  let mut pixmap = tiny_skia::Pixmap::new(width, height).context("Allocate SVG raster")?;
  let transform = tiny_skia::Transform::from_scale(
    width as f32 / tree.size().width(),
    height as f32 / tree.size().height(),
  );
  resvg::render(&tree, transform, &mut pixmap.as_mut());
  // tiny-skia pixels are premultiplied; PNG exports must be straight alpha.
  let data = pixmap
    .pixels()
    .iter()
    .flat_map(|p| {
      let p = p.demultiply();
      [p.red(), p.green(), p.blue(), p.alpha()]
    })
    .collect();
  RgbaImage::from_raw(width, height, data).context("SVG RGBA dimensions")
}
fn source_path(name: &str) -> PathBuf {
  if name == "only" {
    PathBuf::from(LOGO_ONLY)
  } else {
    PathBuf::from(format!("{SOURCE}/logo-{name}.svg"))
  }
}
fn source(name: &str) -> Result<Vec<u8>> {
  Ok(fs::read(source_path(name))?)
}
fn png(image: &RgbaImage) -> Result<Vec<u8>> {
  let mut cursor = Cursor::new(Vec::new());
  image.write_to(&mut cursor, image::ImageFormat::Png)?;
  Ok(cursor.into_inner())
}
fn write_image(path: impl AsRef<Path>, image: &RgbaImage) -> Result<()> {
  image.save(path)?;
  Ok(())
}
fn tracked_images() -> Result<Vec<PathBuf>> {
  let mut paths = Vec::new();
  for dir in ["icons", "artifacts/store/logos"] {
    for entry in fs::read_dir(dir)? {
      let path = entry?.path();
      if path.extension().is_some_and(|e| e == "png") {
        paths.push(path);
      }
    }
  }
  paths.extend([PathBuf::from("icons/icon.ico"), PathBuf::from(LOGO_ONLY)]);
  for name in ["full", "small"] {
    paths.push(source_path(name));
  }
  paths.sort();
  Ok(paths)
}
/// Lowercase hex SHA-1, as `git hash-object` and the provenance records write it.
pub(crate) fn sha1_hex(bytes: &[u8]) -> String {
  Sha1::digest(bytes)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}
fn fingerprint(path: &Path) -> Result<String> {
  Ok(sha1_hex(&fs::read(path)?))
}
fn record(group: &str) -> Result<()> {
  let mut manifest = json(PROVENANCE)?;
  let records = tracked_images()?
    .iter()
    .filter(|path| {
      let path = path.to_string_lossy().replace('\\', "/");
      if group == "app" {
        path.starts_with("icons/") || path == LOGO_ONLY
      } else {
        path.starts_with("artifacts/store/logos/") || path.starts_with(SOURCE) || path == LOGO_ONLY
      }
    })
    .map(|path| {
      Ok((
        path.to_string_lossy().replace('\\', "/"),
        fingerprint(path)?,
      ))
    })
    .collect::<Result<BTreeMap<_, _>>>()?;
  let entries = manifest["sha1"]
    .as_object_mut()
    .context("Branding fingerprints")?;
  for (name, hash) in records {
    entries.insert(name, hash.into());
  }
  let sources = ["full", "only", "small"]
    .iter()
    .map(|name| Ok(((*name).to_owned(), fingerprint(&source_path(name))?)))
    .collect::<Result<BTreeMap<_, _>>>()?;
  manifest[format!("{group}Sources")] = serde_json::to_value(sources)?;
  fs::write(PROVENANCE, serde_json::to_string_pretty(&manifest)? + "\n")?;
  Ok(())
}
fn verify() -> Result<()> {
  let manifest = json(PROVENANCE)?;
  for group in ["app", "store"] {
    for name in ["full", "only", "small"] {
      ensure!(
        manifest[format!("{group}Sources")][name].as_str()
          == Some(fingerprint(&source_path(name))?.as_str()),
        "{group} artwork was not generated from current {name} SVG"
      );
    }
  }
  let records = manifest["sha1"]
    .as_object()
    .context("Branding fingerprints")?;
  let images = tracked_images()?;
  ensure!(
    images.len() == records.len(),
    "Branding asset inventory changed; regenerate and review explicitly"
  );
  for path in images {
    let name = path.to_string_lossy().replace('\\', "/");
    ensure!(
      records.get(&name).and_then(|v| v.as_str()) == Some(fingerprint(&path)?.as_str()),
      "Branding source/export changed: {name}. Regenerate app/Store assets and review."
    );
  }
  let ico = fs::read("icons/icon.ico")?;
  let frames = crate::pe::ico_frames(&ico)?;
  ensure!(frames.len() == 7, "Expected seven ICO sizes");
  for (bytes, size) in frames.iter().zip([16, 24, 32, 48, 64, 128, 256]) {
    ensure!(
      image::load_from_memory(bytes)?.to_rgba8().dimensions() == (size, size),
      "Wrong ICO dimensions"
    );
  }
  println!("Reviewed branding sources/exports and seven ICO frames verified");
  Ok(())
}
fn app() -> Result<()> {
  for entry in fs::read_dir("icons")? {
    let path = entry?.path();
    if path.extension().is_none_or(|e| e != "png")
      || path.file_name().is_some_and(|n| n == "tray.png")
    {
      continue;
    }
    let (w, h) = image::image_dimensions(&path)?;
    let name = path.file_name().context("Icon name")?.to_string_lossy();
    let svg = source(if name.contains("altform-") {
      if w <= 32 { "small" } else { "only" }
    } else {
      "full"
    })?;
    let mut image = raster(&svg, w, h)?;
    if name.starts_with("InstallerLogo") {
      let mut bg = RgbaImage::from_pixel(w, h, Rgba([19, 19, 20, 255]));
      imageops::overlay(&mut bg, &image, 0, 0);
      image = bg;
    }
    write_image(path, &image)?;
  }
  let tray = read(format!("{SOURCE}/logo-small.svg"))?
    .replace("viewBox=\"0 0 16 16\"", "viewBox=\"1 1 14 14\"");
  write_image("icons/tray.png", &raster(tray.as_bytes(), 32, 32)?)?;
  let sizes = [16u32, 24, 32, 48, 64, 128, 256];
  let frames = sizes
    .iter()
    .map(|size| png(&raster(&source("full")?, *size, *size)?))
    .collect::<Result<Vec<_>>>()?;
  let mut directory = vec![0u8; 6 + 16 * frames.len()];
  directory[2..4].copy_from_slice(&1u16.to_le_bytes());
  directory[4..6].copy_from_slice(&(frames.len() as u16).to_le_bytes());
  let mut offset = directory.len();
  for (i, frame) in frames.iter().enumerate() {
    let at = 6 + i * 16;
    directory[at] = sizes[i] as u8;
    directory[at + 1] = sizes[i] as u8;
    directory[at + 4..at + 6].copy_from_slice(&1u16.to_le_bytes());
    directory[at + 6..at + 8].copy_from_slice(&32u16.to_le_bytes());
    directory[at + 8..at + 12].copy_from_slice(&(frame.len() as u32).to_le_bytes());
    directory[at + 12..at + 16].copy_from_slice(&(offset as u32).to_le_bytes());
    offset += frame.len();
  }
  for frame in frames {
    directory.extend(frame);
  }
  fs::write("icons/icon.ico", directory)?;
  record("app")
}
fn store() -> Result<()> {
  for size in [71, 150, 300] {
    write_image(
      format!("artifacts/store/logos/app-icon-{size}.png"),
      &raster(&source("full")?, size, size)?,
    )?;
  }
  record("store")
}
pub fn run(task: &str) -> Result<()> {
  match task {
    "assets:app" => app(),
    "assets:store" => store(),
    _ => verify(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn svg_raster_retains_transparency_and_dimensions() -> Result<()> {
    let image = raster(br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="4" height="4" fill="red"/></svg>"#,16,16)?;
    assert_eq!(image.dimensions(), (16, 16));
    assert_eq!(image.get_pixel(2, 2), &Rgba([255, 0, 0, 255]));
    assert_eq!(image.get_pixel(15, 15)[3], 0);
    Ok(())
  }
}
