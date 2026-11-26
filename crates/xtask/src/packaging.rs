use crate::{process, read, version};
use anyhow::{Context, Result, ensure};
use sha1::{Digest, Sha1};
use std::{
  fs,
  time::{SystemTime, UNIX_EPOCH},
};

pub fn unique() -> String {
  let time = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |d| d.as_nanos());
  format!("{}-{time}", std::process::id())
}

pub fn product_code(version: &str) -> Result<String> {
  ensure!(
    regex::Regex::new(r"^\d+\.\d+\.\d+$")?.is_match(version),
    "Invalid installer version"
  );
  let namespace = [
    0x88, 0x6d, 0xc4, 0x6e, 0x70, 0xd6, 0x4f, 0x71, 0xb8, 0xd6, 0xcc, 0x07, 0x30, 0x21, 0x78, 0x7a,
  ];
  let mut hash = Sha1::new();
  hash.update(namespace);
  hash.update(format!("Flinch.Installer.MSI/{version}"));
  let mut bytes = hash.finalize()[..16].to_vec();
  bytes[6] = (bytes[6] & 15) | 0x50;
  bytes[8] = (bytes[8] & 63) | 0x80;
  let hex = bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
  Ok(format!(
    "{{{}-{}-{}-{}-{}}}",
    &hex[..8],
    &hex[8..12],
    &hex[12..16],
    &hex[16..20],
    &hex[20..]
  ))
}

pub fn installer(task: &str) -> Result<()> {
  let version = version()?;
  process::powershell(
    "scripts/installer.ps1",
    &[
      "-Action".into(),
      match task {
        "pack:installer" => "Build",
        "test:installer:plan" => "Plan",
        _ => "Startup",
      }
      .into(),
      "-Version".into(),
      version.clone(),
      "-ProductCode".into(),
      product_code(&version)?,
    ],
  )
}

pub fn msix(options: &[String]) -> Result<()> {
  let plan = msix_options(options)?;
  let mut args = vec!["-Version".into(), version()?];
  for flag in ["--local-test", "--stage-only", "--skip-build"] {
    if options.iter().any(|s| s == flag) {
      args.push(
        match flag {
          "--local-test" => "-LocalTest",
          "--stage-only" => "-StageOnly",
          _ => "-SkipBuild",
        }
        .into(),
      );
    }
  }
  args.extend(["-Revision".into(), plan.to_string()]);
  process::powershell("scripts/pack-msix.ps1", &args)
}

pub fn msix_options(options: &[String]) -> Result<u16> {
  let mut seen = std::collections::BTreeSet::new();
  let mut revision = 0;
  let mut iter = options.iter();
  while let Some(arg) = iter.next() {
    ensure!(seen.insert(arg), "Duplicate MSIX option {arg}");
    match arg.as_str() {
      "--local-test" | "--stage-only" | "--skip-build" => (),
      "--revision" => {
        revision = iter
          .next()
          .context("Missing revision")?
          .parse::<u16>()
          .context("Revision must be 1..65535")?;
        ensure!(revision > 0, "Revision must be positive");
      },
      _ => anyhow::bail!("Unknown MSIX option {arg}"),
    }
  }
  ensure!(
    !seen.contains(&"--revision".to_owned()) || seen.contains(&"--local-test".to_owned()),
    "Revision requires local test"
  );
  ensure!(
    !seen.contains(&"--skip-build".to_owned()) || seen.contains(&"--stage-only".to_owned()),
    "Skipping build is only allowed for staging"
  );
  Ok(revision)
}

pub fn sync_version(options: &[String]) -> Result<()> {
  ensure!(options.len() == 1, "version:sync requires x.y.z");
  let version = &options[0];
  ensure!(
    regex::Regex::new(r"^\d+\.\d+\.\d+$")?.is_match(version)
      && version.split('.').all(|p| p.parse::<u16>().is_ok()),
    "Version components must be <=65535"
  );
  // Validate every replacement before writing any source.
  let mut updates = Vec::new();
  let pattern = regex::Regex::new(r#"(?m)^version = "[^"]+"$"#)?;
  for file in [
    "Cargo.toml",
    "crates/settings/Cargo.toml",
    "crates/installer/Cargo.toml",
  ] {
    let text = read(file)?;
    ensure!(pattern.is_match(&text), "Missing version in {file}");
    updates.push((
      file,
      pattern
        .replacen(&text, 1, format!("version = \"{version}\""))
        .into_owned(),
    ));
  }
  let manifest = read("appxmanifest.xml")?;
  let identity = regex::Regex::new(r#"(<Identity\b[^>]*\bVersion=")[^"]+("[^>]*>)"#)?;
  ensure!(identity.is_match(&manifest), "Missing MSIX version");
  updates.push((
    "appxmanifest.xml",
    identity
      .replace(&manifest, format!("${{1}}{version}.0${{2}}"))
      .into_owned(),
  ));
  for (file, text) in updates {
    fs::write(file, text)?;
  }
  // Refresh workspace package records without network access.
  process::cargo(&[
    "metadata",
    "--offline",
    "--no-deps",
    "--format-version",
    "1",
  ])
}

pub fn validate_runtime(metadata: &serde_json::Value) -> Result<()> {
  ensure!(
    metadata["status"] == "Valid"
      && regex::Regex::new(r"(?:^|,\s*)O=Microsoft Corporation(?:,|$)")?
        .is_match(metadata["subject"].as_str().unwrap_or_default()),
    "Microsoft Authenticode signature required"
  );
  ensure!(
    regex::Regex::new(r"\A14\.\d+\.\d+\.\d+\z")?
      .is_match(metadata["version"].as_str().unwrap_or_default()),
    "Supported v14 version required"
  );
  ensure!(
    metadata["description"]
      .as_str()
      .unwrap_or_default()
      .to_lowercase()
      .contains("(x64)"),
    "x64 redistributable required"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn stable_msi_identity_survives_migration() -> Result<()> {
    assert_eq!(
      product_code("1.1.0")?,
      "{25D62DA1-F217-586D-B92F-AEBEAA30ED12}"
    );
    assert_ne!(product_code("1.1.0")?, product_code("1.1.1")?);
    assert!(product_code("bad-version").is_err());
    Ok(())
  }
  #[test]
  fn msix_rejects_unsafe_or_ambiguous_options() {
    for args in [
      vec!["--skip-build"],
      vec!["--revision", "2"],
      vec!["--local-test", "--revision", "0"],
      vec!["--stage-only", "--stage-only"],
      vec!["--local-test", "--revision", "65536"],
      vec!["--unknown"],
    ] {
      assert!(msix_options(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
    }
  }
  #[test]
  fn runtime_metadata_fails_closed() {
    let valid = serde_json::json!({"status":"Valid", "subject":"CN=Microsoft Corporation, O=Microsoft Corporation, C=US", "version":"14.50.35719.0", "description":"Microsoft Visual C++ Redistributable (x64)"});
    assert!(validate_runtime(&valid).is_ok());
    for (key, value) in [
      ("status", "NotSigned"),
      ("status", "HashMismatch"),
      ("status", "UnknownError"),
      ("subject", "O=Other Corporation"),
      ("description", "Visual C++ (x86)"),
      ("version", "13.0.0.0"),
      ("version", "14.50"),
      ("version", "14.50.35719.0\nBAD"),
    ] {
      let mut invalid = valid.clone();
      invalid[key] = value.into();
      assert!(validate_runtime(&invalid).is_err());
    }
  }
}
