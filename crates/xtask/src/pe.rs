//! Checked PE/ICO parsing; never loads the inspected executable.
use anyhow::{Context, Result, bail, ensure};
use std::{fs, path::Path};
fn u16le(data: &[u8], offset: usize) -> Result<u16> {
  Ok(u16::from_le_bytes(
    data
      .get(offset..offset + 2)
      .context("Truncated u16")?
      .try_into()?,
  ))
}
fn u32le(data: &[u8], offset: usize) -> Result<usize> {
  Ok(u32::from_le_bytes(
    data
      .get(offset..offset + 4)
      .context("Truncated u32")?
      .try_into()?,
  ) as usize)
}
struct Pe {
  data: Vec<u8>,
  sections: usize,
  count: usize,
  directories: usize,
}
impl Pe {
  fn load(file: &Path) -> Result<Self> {
    let data = fs::read(file)?;
    let nt = u32le(&data, 0x3c)?;
    ensure!(data.get(nt..nt + 4) == Some(b"PE\0\0"), "Invalid PE");
    let optional = nt + 24;
    let magic = u16le(&data, optional)?;
    ensure!(
      magic == 0x20b || magic == 0x10b,
      "Unknown PE optional header"
    );
    let directories = optional + if magic == 0x20b { 112 } else { 96 };
    let sections = optional + usize::from(u16le(&data, nt + 20)?);
    let count = usize::from(u16le(&data, nt + 6)?);
    Ok(Self {
      data,
      sections,
      count,
      directories,
    })
  }
  fn offset(&self, rva: usize) -> Result<usize> {
    for i in 0..self.count {
      let at = self.sections + i * 40;
      let start = u32le(&self.data, at + 12)?;
      let size = u32le(&self.data, at + 8)?.max(u32le(&self.data, at + 16)?);
      if rva >= start && rva - start < size {
        return Ok(u32le(&self.data, at + 20)? + rva - start);
      }
    }
    bail!("Unmapped PE RVA {rva}")
  }
  fn entries(&self, base: usize, relative: usize) -> Result<Vec<(usize, usize)>> {
    let at = base + relative;
    let length =
      usize::from(u16le(&self.data, at + 12)?) + usize::from(u16le(&self.data, at + 14)?);
    (0..length)
      .map(|i| {
        Ok((
          u32le(&self.data, at + 16 + i * 8)?,
          u32le(&self.data, at + 20 + i * 8)? & 0x7fffffff,
        ))
      })
      .collect()
  }
}
pub fn ico_frames(data: &[u8]) -> Result<Vec<&[u8]>> {
  ensure!(
    u16le(data, 0)? == 0 && u16le(data, 2)? == 1,
    "Invalid ICO header"
  );
  let count = usize::from(u16le(data, 4)?);
  ensure!(count > 0, "Empty ICO");
  (0..count)
    .map(|i| {
      let start = u32le(data, 6 + i * 16 + 12)?;
      let size = u32le(data, 6 + i * 16 + 8)?;
      ensure!(start >= 6 + count * 16, "ICO overlaps header");
      data
        .get(start..start.checked_add(size).context("ICO overflow")?)
        .context("Truncated ICO frame")
    })
    .collect()
}
pub fn branding(file: &Path) -> Result<()> {
  let pe = Pe::load(file)?;
  let base = pe.offset(u32le(&pe.data, pe.directories + 16)?)?;
  let mut resources = Vec::new();
  for (kind, pointer) in pe.entries(base, 0)? {
    if kind != 3 {
      continue;
    }
    for (_, name) in pe.entries(base, pointer)? {
      for (_, language) in pe.entries(base, name)? {
        let start = pe.offset(u32le(&pe.data, base + language)?)?;
        let size = u32le(&pe.data, base + language + 4)?;
        resources.push(
          pe.data
            .get(start..start.checked_add(size).context("Resource overflow")?)
            .context("Truncated resource")?,
        );
      }
    }
  }
  let ico = fs::read("icons/icon.ico")?;
  for frame in ico_frames(&ico)? {
    ensure!(
      resources.contains(&frame),
      "Executable icon differs from the app ICO"
    );
  }
  println!("Executable ICO resources match");
  Ok(())
}
pub fn imports(file: &Path) -> Result<()> {
  let pe = Pe::load(file)?;
  let mut names = Vec::new();
  for (index, stride, name_offset) in [(1, 20, 12), (13, 32, 4)] {
    let rva = u32le(&pe.data, pe.directories + index * 8)?;
    let size = u32le(&pe.data, pe.directories + index * 8 + 4)?;
    if rva == 0 {
      continue;
    }
    let start = pe.offset(rva)?;
    for at in (start..(start + size).min(pe.data.len())).step_by(stride) {
      if at + stride > pe.data.len().min(start + size) {
        break;
      }
      let name_rva = u32le(&pe.data, at + name_offset)?;
      if name_rva == 0 {
        break;
      }
      if index == 13 {
        ensure!(
          u32le(&pe.data, at)? & 1 != 0,
          "VA-based delay imports unsupported"
        );
      }
      let name = pe.offset(name_rva)?;
      let end = pe.data[name..]
        .iter()
        .position(|b| *b == 0)
        .context("Unterminated import")?;
      names.push(String::from_utf8(pe.data[name..name + end].to_vec())?.to_lowercase());
    }
  }
  let system = [
    "kernel32.dll",
    "user32.dll",
    "gdi32.dll",
    "gdiplus.dll",
    "advapi32.dll",
    "shell32.dll",
    "shlwapi.dll",
    "ole32.dll",
    "oleaut32.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "version.dll",
    "wininet.dll",
    "winhttp.dll",
    "crypt32.dll",
    "bcrypt.dll",
    "msi.dll",
    "userenv.dll",
    "rpcrt4.dll",
    "ws2_32.dll",
    "ntdll.dll",
    "secur32.dll",
    "wintrust.dll",
    "uxtheme.dll",
    "dwmapi.dll",
    "cabinet.dll",
  ];
  ensure!(
    !names.is_empty()
      && names.iter().all(|s| system.contains(&s.as_str())
        || s.starts_with("api-ms-win-")
        || s.starts_with("ext-ms-win-")),
    "Prerequisite has non-system imports: {names:?}"
  );
  Ok(())
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn rejects_truncated_ico() {
    assert!(ico_frames(&[]).is_err());
    assert!(ico_frames(&[0, 0, 1, 0, 1, 0]).is_err());
  }
}
