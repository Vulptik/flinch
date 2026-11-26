use crate::{json, read, tasks, version};
use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde_json::Value;
use std::{
  collections::BTreeSet,
  fs,
  path::{Path, PathBuf},
};

fn re(pattern: &str) -> Result<Regex> {
  Regex::new(pattern).context("Compile contract expression")
}
fn require(text: &str, tokens: &[&str], name: &str) -> Result<()> {
  for token in tokens {
    ensure!(text.contains(token), "{name}: missing {token}");
  }
  Ok(())
}
fn files(dir: &str) -> Result<Vec<PathBuf>> {
  let mut found = Vec::new();
  for entry in fs::read_dir(dir)? {
    let path = entry?.path();
    if path.is_dir() {
      found.extend(files(&path.to_string_lossy())?);
    } else {
      found.push(path);
    }
  }
  Ok(found)
}
fn keys(value: &Value) -> Result<BTreeSet<&str>> {
  Ok(
    value
      .as_object()
      .context("Expected catalog object")?
      .keys()
      .map(String::as_str)
      .collect(),
  )
}
pub fn docs() -> Result<()> {
  let docs = [
    "README.md",
    "CONTRIBUTING.md",
    "NOTICE.md",
    "TRADEMARKS.md",
    "SECURITY.md",
    "docs/architecture.md",
  ];
  let commands = re(r"\bcargo xtask ([\w:-]+)")?;
  let links = re(r#"!?\[[^\]]*\]\(([^\s)]+)(?:\s+"[^"]*")?\)"#)?;
  let mut dated = Vec::new();
  for file in docs {
    let text = read(file)?;
    for cap in commands.captures_iter(&text) {
      ensure!(
        tasks::COMMANDS.contains(&&cap[1]),
        "Unknown task {} in {file}",
        &cap[1]
      );
    }
    for cap in links.captures_iter(&text) {
      let target = cap[1]
        .trim_matches(['<', '>'])
        .split('#')
        .next()
        .context("Link")?;
      if target.is_empty() || target.contains(':') || target.starts_with('/') {
        continue;
      }
      let path = Path::new(file).parent().context("Doc parent")?.join(target);
      ensure!(path.exists(), "Missing Markdown link {target} in {file}");
    }
    let lines = dated_lines(&text)?;
    if !lines.is_empty() {
      dated.push(format!("{file} (lines {lines:?})"));
    }
  }
  ensure!(
    dated.is_empty(),
    "Dated notes in docs: {}. Docs state the current rule, without dates",
    dated.join("; ")
  );
  println!("Documentation passed");
  Ok(())
}

/// Lines (1-based) that carry a date outside inline code and link targets, where paths such
/// as dated submission folders may name one.
fn dated_lines(text: &str) -> Result<Vec<usize>> {
  let date = re(r"\b20\d{2}-\d{2}-\d{2}\b")?;
  let paths = re(r"`[^`]*`|\]\([^)]*\)")?;
  Ok(
    text
      .lines()
      .enumerate()
      .filter(|(_, line)| date.is_match(&paths.replace_all(line, "")))
      .map(|(index, _)| index + 1)
      .collect(),
  )
}

pub fn settings_coverage() -> Result<()> {
  let pages = read("crates/settings/ui/pages.slint")?;
  let tests = read("crates/settings/src/interaction_check.rs")?;
  for (registry, pattern) in [
    ("FIELDS", r#"edit-request\("([^"]+)"\)"#),
    ("ACTIONS", r#"SettingsState\.action\("([^"]+)"\)"#),
  ] {
    let capture = re(&format!(r"pub const {registry}:[\s\S]*?= &\[([\s\S]*?)\];"))?;
    let body = capture
      .captures(&tests)
      .context("Missing interaction registry")?;
    let ui: BTreeSet<_> = re(pattern)?
      .captures_iter(&pages)
      .map(|c| c[1].to_owned())
      .collect();
    let covered: BTreeSet<_> = re(r#""([^"]+)""#)?
      .captures_iter(&body[1])
      .map(|c| c[1].to_owned())
      .collect();
    ensure!(
      ui == covered,
      "Settings {registry} coverage differs: UI={ui:?}, tests={covered:?}"
    );
    let cases = &tests[body.get(0).context("Registry match")?.end()..];
    for field in covered {
      ensure!(
        cases.contains(&format!("\"{field}\"")),
        "Missing executed case {field}"
      );
    }
  }
  for cap in re(r#"capture-request\("([^"]+)"\)"#)?.captures_iter(&pages) {
    ensure!(
      tests.contains(&format!("\"{}\"", &cap[1])),
      "Missing shortcut case"
    );
  }
  println!("Settings interaction inventory passed");
  Ok(())
}

fn config() -> Result<()> {
  let source = read("src/config/mod.rs")?;
  let start = source.find("pub struct Config").context("Config struct")?;
  let end = source
    .find("static PENDING_LOAD_WARNING")
    .context("Config end")?;
  let fields: BTreeSet<_> = re(r"pub\s+(\w+)\s*:")?
    .captures_iter(&source[start..end])
    .map(|c| c[1].to_owned())
    .collect();
  for key in keys(&json("resources/defaultConfig.json")?)? {
    ensure!(fields.contains(key), "Unknown default {key}");
  }
  let mut native = String::new();
  for dir in [
    "src",
    "crates/settings/src",
    "crates/settings/ui",
    "crates/settings/examples",
    "crates/video-lab/src",
    "crates/video-engine/src",
  ] {
    for file in files(dir)? {
      if file.extension().is_some_and(|e| e == "rs" || e == "slint") {
        native.push_str(&read(file)?);
      }
    }
  }
  native.push_str(&read("crates/settings/build.rs")?);
  let reference = json("messages/en.json")?;
  for locale in ["en", "fr", "de", "es", "it"] {
    let catalog = json(format!("messages/{locale}.json"))?;
    ensure!(
      keys(&catalog)? == keys(&reference)?,
      "Catalog keys differ: {locale}"
    );
    for key in keys(&reference)? {
      ensure!(
        key != "$schema"
          && native.contains(key)
          && catalog[key].as_str().is_some_and(|s| !s.trim().is_empty()),
        "Unused/empty {locale} message {key}"
      );
    }
  }
  Ok(())
}

fn installer_shutdown_contract(xml: &str, element: &str, name_attribute: &str) -> Result<()> {
  for (property, value) in [
    ("MSIRESTARTMANAGERCONTROL", "0"),
    ("MSIRMSHUTDOWN", "1"),
    ("MSIDISABLERMRESTART", "1"),
  ] {
    let pattern = format!(r#"<{element}\s+{name_attribute}="{property}"\s+Value="{value}"\s*/>"#);
    ensure!(
      re(&pattern)?.is_match(xml),
      "MSI maintenance shutdown policy requires {property}={value}"
    );
  }
  Ok(())
}

pub fn installer() -> Result<()> {
  let catalogs = json("messages/installer-ui.json")?;
  ensure!(
    keys(&catalogs)? == BTreeSet::from(["en", "fr", "de", "es", "it"]),
    "Installer locales differ"
  );
  for locale in ["en", "fr", "de", "es", "it"] {
    ensure!(
      keys(&catalogs[locale])? == keys(&catalogs["en"])?,
      "Installer keys differ: {locale}"
    );
    for key in keys(&catalogs[locale])? {
      ensure!(
        catalogs[locale][key]
          .as_str()
          .is_some_and(|s| !s.trim().is_empty()),
        "Empty installer copy {locale}/{key}"
      );
    }
  }
  let bundle = read("windows/installer/Bundle.wxs")?;
  installer_shutdown_contract(&bundle, "MsiProperty", "Name")?;
  require(
    &bundle,
    &[
      "WixPrerequisiteBootstrapperApplication",
      "bal:PrereqPackage=\"yes\"",
      "Permanent=\"yes\"",
      "RuntimeInstalled = 1",
      "RuntimeVersion &gt;= v$(var.VcVersion)",
      "Flinch",
      "EnableFeatureSelection=\"yes\"",
      "MsiProperty Name=\"INSTALLFOLDER\"",
    ],
    "Burn",
  )?;
  ensure!(re(r#"<BootstrapperApplication\b(?:[^>]*Id="FlinchUI"[^>]*Secondary="yes"|[^>]*Secondary="yes"[^>]*Id="FlinchUI")[^>]*>"#)?.is_match(&bundle), "Custom UI must be secondary");
  let msi = read("windows/installer/Package.wxs")?;
  installer_shutdown_contract(&msi, "Property", "Id")?;
  require(
    &msi,
    &[
      "Scope=\"perUserOrMachine\"",
      "Root=\"HKMU\"",
      "Id=\"DesktopFeature\"",
      "Id=\"StartMenuFeature\"",
      "Id=\"INSTALLFOLDER\"",
      "MajorUpgrade",
      "REINSTALLMODE",
      "Flinch",
    ],
    "MSI",
  )?;
  ensure!(
    !re("AppDataFolder|RemoveFile|RemoveFolderEx|<Level Value=\"0\"")?.is_match(&msi),
    "MSI must preserve data and selectable shortcuts"
  );
  require(
    &read("crates/installer/Cargo.toml")?,
    &[
      "required-features = [\"ui\"]",
      &format!("version = \"{}\"", version()?),
    ],
    "Installer manifest",
  )?;
  let bridge = read("windows/installer/burn_bridge.cpp")?;
  require(
    &bridge,
    &[
      "selectedScope{BOOTSTRAPPER_SCOPE_PER_USER}",
      "flinch_burn_set_options",
      "IsWindow(parent)",
      "Apply(parent)",
      "flinch_burn_set_parent",
      "OnPlanMsiFeature",
      "current->Detect(parentWindow.load())",
    ],
    "Burn bridge",
  )?;
  ensure!(!bridge.contains("Apply(nullptr)"), "Missing Apply parent");
  let host = read("crates/installer/src/main.rs")?;
  require(&host, &["set_minimized(false)"], "Installer host")?;
  ensure!(
    re(r#"Instance::acquire_named\(\s*"io\.github\.vulptik\.flinch\.installer"\s*,"#)?
      .is_match(&host),
    "Installer must retain its distinct single-instance identity"
  );
  ensure!(
    !re(r"Installer Preview|Installer preview|Prototype|Prototipo|Prototyp|installer\.preview")?
      .is_match(&format!("{bundle}{msi}{host}{catalogs}")),
    "Retired installer branding"
  );
  Ok(())
}

fn packaging() -> Result<()> {
  let version = version()?;
  for file in ["crates/settings/Cargo.toml", "crates/installer/Cargo.toml"] {
    require(&read(file)?, &[&format!("version = \"{version}\"")], file)?;
  }
  for svg in [
    "crates/settings/ui/assets/logo.svg",
    "icons/source/logo-full.svg",
    "icons/source/logo-small.svg",
  ] {
    ensure!(Path::new(svg).is_file(), "Missing branding source {svg}");
  }
  require(
    &read("build.rs")?,
    &["cargo:rerun-if-changed=icons/icon.ico"],
    "Branding dependency",
  )?;
  // The native engine records every video and GIF; a bundled FFmpeg must not return.
  ensure!(
    !Path::new("ffmpeg").exists(),
    "Flinch records with its native engine; remove the ffmpeg folder"
  );
  for file in [
    "build.rs",
    "scripts/pack-msix.ps1",
    "scripts/installer.ps1",
    "scripts/tooling.ps1",
  ] {
    ensure!(
      !read(file)?.to_ascii_lowercase().contains("ffmpeg"),
      "Flinch records with its native engine; {file} must not package ffmpeg"
    );
  }
  // Installed direct-download builds check the latest GitHub release's version.json.
  require(
    &read("scripts/installer.ps1")?,
    &[
      "$build/version.json",
      "'{\"version\":\"' + $Version + '\"}'",
    ],
    "Installer release manifest",
  )?;
  let manifest = read("appxmanifest.xml")?;
  let store = json("resources/store-identity.json")?;
  let identity = re(r"<Identity\s[^>]*/>")?
    .find(&manifest)
    .context("MSIX identity")?
    .as_str();
  for (attribute, value) in [
    (
      "Name",
      store["packageName"]
        .as_str()
        .context("Package name")?
        .to_owned(),
    ),
    (
      "Publisher",
      store["publisher"].as_str().context("Publisher")?.to_owned(),
    ),
    ("Version", format!("{version}.0")),
    ("ProcessorArchitecture", "x64".into()),
  ] {
    require(
      identity,
      &[&format!("{attribute}=\"{value}\"")],
      "MSIX identity",
    )?;
  }
  require(
    &manifest,
    &[
      "<Logo>Assets\\InstallerLogo.png</Logo>",
      &format!(
        "<PublisherDisplayName>{}</PublisherDisplayName>",
        store["publisherDisplayName"]
          .as_str()
          .context("Publisher display name")?
      ),
    ],
    "MSIX properties",
  )?;
  ensure!(
    re(r"^[A-Z0-9]{12}$")?.is_match(store["storeId"].as_str().context("Store ID")?),
    "Store ID invalid"
  );
  let autostart = read("src/autostart.rs")?;
  let task = re(r#"TASK_ID: &str = "([^"]+)""#)?
    .captures(&autostart)
    .context("Startup task ID")?[1]
    .to_owned();
  let extension =
    re(r#"<uap5:Extension\s[^>]*Category="windows\.startupTask"[^>]*>[\s\S]*?</uap5:Extension>"#)?
      .find(&manifest)
      .context("Startup extension")?
      .as_str();
  require(
    extension,
    &[
      "Executable=\"flinch.exe\"",
      "EntryPoint=\"Windows.FullTrustApplication\"",
      "Enabled=\"false\"",
      &format!("TaskId=\"{task}\""),
    ],
    "MSIX autostart",
  )?;
  let mut pngs = vec![
    ("InstallerLogo.png".to_owned(), 50),
    ("InstallerLogo.scale-300.png".to_owned(), 150),
  ];
  for size in [16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256] {
    for suffix in ["", "_altform-unplated", "_altform-lightunplated"] {
      pngs.push((
        format!("Square44x44Logo.targetsize-{size}{suffix}.png"),
        size,
      ));
    }
  }
  for (name, size) in pngs {
    ensure!(
      image::image_dimensions(Path::new("icons").join(&name))? == (size, size),
      "Wrong dimensions: {name}"
    );
  }
  if let Ok(tag) = std::env::var("GITHUB_REF") {
    ensure!(
      !tag.starts_with("refs/tags/") || tag == format!("refs/tags/v{version}"),
      "Tag {tag} differs from {version}"
    );
  }
  Ok(())
}

fn native() -> Result<()> {
  require(
    &read("Cargo.toml")?,
    &[
      "default = [\"native-settings\", \"desktop-app\"]",
      "required-features = [\"desktop-app\"]",
      "\"crates/xtask\"",
    ],
    "Cargo workspace",
  )?;
  require(
    &read("src/app/instance.rs")?,
    &["io.github.vulptik.flinch"],
    "Instance identity",
  )?;
  Ok(())
}

/// A reviewed third-party asset folder: unmodified files fingerprinted in references.json.
struct Vendored {
  name: &'static str,
  package: &'static str,
  license: &'static str,
  license_file: &'static str,
  notice: &'static str,
  extension: &'static str,
}

const FLUENT_ICONS: Vendored = Vendored {
  name: "Fluent icon",
  package: "@fluentui/svg-icons",
  license: "MIT",
  license_file: "LICENSE",
  notice: "Copyright (c) 2020 Microsoft Corporation",
  extension: ".svg",
};

const CHROMIUM_CURSORS: Vendored = Vendored {
  name: "Chromium cursor",
  package: "chromium/src ui/resources/cursors",
  license: "BSD-3-Clause",
  license_file: "CHROMIUM-LICENSE",
  notice: "Copyright 2015 The Chromium Authors",
  extension: ".cur",
};

/// Vendored assets stay byte-identical to the fingerprints recorded when they were reviewed.
fn vendored(dir: &Path, set: &Vendored) -> Result<()> {
  let name = set.name;
  let manifest: Value = serde_json::from_str(&fs::read_to_string(dir.join("references.json"))?)
    .with_context(|| format!("{name} references"))?;
  ensure!(
    manifest["license"] == set.license && manifest["package"] == set.package,
    "{name} references must name the {} {} package",
    set.license,
    set.package
  );
  ensure!(
    fs::read_to_string(dir.join(set.license_file))?.contains(set.notice),
    "{name} license notice is missing"
  );
  let listed = manifest["sha1"]
    .as_object()
    .with_context(|| format!("{name} fingerprints"))?;
  let mut present = BTreeSet::new();
  for entry in fs::read_dir(dir)? {
    let file = entry?.file_name().to_string_lossy().into_owned();
    if file.ends_with(set.extension) {
      present.insert(file);
    } else {
      ensure!(
        file == set.license_file || file == "references.json",
        "Unexpected file beside {name}s: {file}"
      );
    }
  }
  let expected: BTreeSet<String> = listed.keys().cloned().collect();
  ensure!(
    present == expected,
    "{name} inventory changed; review new or removed files and update references.json"
  );
  for (file, sha1) in listed {
    let actual = crate::assets::sha1_hex(&fs::read(dir.join(file))?);
    ensure!(
      sha1.as_str() == Some(actual.as_str()),
      "{name} {file} differs from its reviewed upstream file"
    );
  }
  Ok(())
}

fn fluent_icons(dir: &Path) -> Result<()> {
  vendored(dir, &FLUENT_ICONS)
}

fn chromium_cursors(dir: &Path) -> Result<()> {
  vendored(dir, &CHROMIUM_CURSORS)
}

fn artifacts() -> Result<()> {
  fluent_icons(Path::new("resources/icons/fluent"))?;
  chromium_cursors(Path::new("resources/cursors"))?;
  let inventory: Vec<String> = serde_json::from_value(json("scripts/repository-artifacts.json")?)?;
  let set: BTreeSet<_> = inventory.iter().collect();
  ensure!(set.len() == inventory.len(), "Duplicate artifact paths");
  for file in &inventory {
    ensure!(
      (file.starts_with("artifacts/store/") || file.starts_with("artifacts/performance/"))
        && !file.contains("..")
        && Path::new(file).exists()
        && !re(r"(?i)\.(exe|dll|msix|msixbundle|msi|pfx|p12|key|zip)$")?.is_match(file),
      "Invalid artifact {file}"
    );
  }
  // Git reads are captured with a finite deadline. Fixed arguments avoid shell interpolation.
  for args in [
    vec!["ls-files", "-z", "--", "artifacts"],
    vec![
      "ls-files",
      "-z",
      "--others",
      "--exclude-standard",
      "--",
      "artifacts",
    ],
  ] {
    let output = crate::process::capture(crate::process::command("git").args(args), 10)?;
    for file in output
      .split('\0')
      .filter(|s| !s.is_empty() && Path::new(s).exists())
    {
      ensure!(set.contains(&file.to_owned()), "Unreviewed artifact {file}");
    }
  }
  // A matching rule plus --non-matching makes check-ignore return success for mixed inputs.
  fn ignored(paths: &[String]) -> Result<BTreeSet<String>> {
    let mut cmd = crate::process::command("git");
    cmd.args(["check-ignore", "--no-index", "--verbose", "--non-matching"]);
    cmd.args(paths);
    let out = crate::process::capture(&mut cmd, 10)?;
    Ok(
      out
        .lines()
        .filter_map(|line| {
          let (rule, path) = line.split_once('\t')?;
          let pattern = rule.splitn(3, ':').nth(2)?;
          if pattern.is_empty() || pattern.starts_with('!') {
            None
          } else {
            Some(path.to_owned())
          }
        })
        .collect(),
    )
  }
  ensure!(
    ignored(&inventory)?.is_empty(),
    "Maintained artifacts ignored"
  );
  let generated: Vec<String> = [
    "artifacts/new-probe/results.json",
    "artifacts/installer/working/build/setup.exe",
    "artifacts/performance/settings-checks/example/results.json",
    "artifacts/performance/unreviewed-baseline.json",
    "artifacts/store/working/build/Flinch.exe",
    "artifacts/store/logos/temporary-preview.png",
  ]
  .map(String::from)
  .to_vec();
  ensure!(
    ignored(&generated)?.len() == generated.len(),
    "Generated outputs exposed to Git"
  );
  Ok(())
}

fn workflow() -> Result<()> {
  let ci = read(".github/workflows/build.yml")?;
  for task in [
    "test:recording",
    "test:settings",
    "test:run --integration-only",
    "test:tooling",
    "test:installer",
  ] {
    require(&ci, &[&format!("cargo xtask {task}")], "Build CI")?;
  }
  // GitHub's Windows Server runners have no interactive desktop, so desktop gates fail there
  // for reasons unrelated to Flinch. They run locally through check:release.
  for workflow in [
    ".github/workflows/build.yml",
    ".github/workflows/release.yml",
    ".github/workflows/single-check.yml",
  ] {
    ensure!(
      !re(r"cargo xtask test:[\w:]*:desktop")?.is_match(&read(workflow)?),
      "{workflow} must not run desktop gates; they belong to the local check:release"
    );
  }
  require(
    &read(".github/workflows/release.yml")?,
    &[
      "cargo xtask test:run --integration-only",
      "cargo xtask test:recording",
      // Tag builds attach the installer to a draft that only the maintainer publishes.
      "cargo xtask pack:installer",
      "actions/attest-build-provenance",
      "--draft --verify-tag",
    ],
    "Release CI",
  )?;
  for workflow in [
    ".github/workflows/build.yml",
    ".github/workflows/release.yml",
  ] {
    ensure!(
      !read(workflow)?.contains("artifacts/store/working/build/staging"),
      "{workflow} must not upload app builds as artifacts"
    );
  }
  for file in [
    "src/commands/clipboard.rs",
    "src/commands/screenshot/post_capture/drive_tests.rs",
    "src/screenshot/platform/windows/wgc.rs",
    "src/video/controller/native_desktop_tests.rs",
  ] {
    let text = read(file)?;
    let cases = re(r"#\[(?:tokio::)?test\]([\s\S]*?)(?:async )?fn (\w+)\(")?;
    let matches: Vec<_> = cases.captures_iter(&text).collect();
    ensure!(
      !matches.is_empty() && matches.iter().all(|c| c[1].contains("#[ignore")),
      "Desktop tests must be ignored: {file}"
    );
  }
  Ok(())
}

pub fn contracts() -> Result<()> {
  crate::coverage::policy_contract()?;
  config()?;
  native()?;
  packaging()?;
  artifacts()?;
  workflow()?;
  installer()?;
  println!(
    "Config, localization, native host, packaging, artifacts, workflow and installer contracts passed"
  );
  Ok(())
}

#[cfg(test)]
mod docs_tests {
  use super::*;

  #[test]
  fn public_docs_carry_no_dates_outside_code() -> Result<()> {
    let text = "Current rule.\nDecided on 2026-11-25.\nBuilt into `builds/1.0.0.0/2026-10-04/`.\n2026-1-5 is no date.\n[`its report`](reports/2026-10-04/report.html)";
    assert_eq!(dated_lines(text)?, [2]);
    Ok(())
  }
  #[test]
  fn chromium_cursors_reject_edited_unlisted_and_unlicensed_files() -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/cursors");
    chromium_cursors(&source)?;
    let tampered = |mutate: &dyn Fn(&Path) -> Result<()>| -> Result<bool> {
      let dir = tempfile::tempdir()?;
      for entry in fs::read_dir(&source)? {
        let entry = entry?;
        fs::copy(entry.path(), dir.path().join(entry.file_name()))?;
      }
      mutate(dir.path())?;
      Ok(chromium_cursors(dir.path()).is_err())
    };
    assert!(tampered(&|dir| Ok(fs::write(
      dir.join("hand_grab.cur"),
      [0u8; 8]
    )?))?);
    assert!(tampered(&|dir| Ok(fs::write(
      dir.join("rotation.cur"),
      [0u8; 8]
    )?))?);
    assert!(tampered(&|dir| Ok(fs::write(
      dir.join("rotation.svg"),
      "<svg/>"
    )?))?);
    assert!(tampered(&|dir| Ok(fs::write(
      dir.join("CHROMIUM-LICENSE"),
      "BSD"
    )?))?);
    assert!(!tampered(&|_| Ok(()))?, "an untouched copy passes");
    Ok(())
  }

  #[test]
  fn fluent_icons_reject_edited_unlisted_missing_and_unlicensed_files() -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/icons/fluent");
    fluent_icons(&source)?;
    let copy = |mutate: &dyn Fn(&Path) -> Result<()>| -> Result<bool> {
      let dir = tempfile::tempdir()?;
      for entry in fs::read_dir(&source)? {
        let entry = entry?;
        fs::copy(entry.path(), dir.path().join(entry.file_name()))?;
      }
      mutate(dir.path())?;
      Ok(fluent_icons(dir.path()).is_err())
    };
    assert!(copy(&|dir| Ok(fs::write(
      dir.join("pen_20_regular.svg"),
      "<svg></svg>"
    )?))?);
    assert!(copy(&|dir| Ok(fs::write(
      dir.join("extra_20_regular.svg"),
      "<svg></svg>"
    )?))?);
    assert!(copy(&|dir| Ok(fs::remove_file(
      dir.join("stop_20_filled.svg")
    )?))?);
    assert!(copy(&|dir| Ok(fs::write(dir.join("LICENSE"), "MIT")?))?);
    assert!(copy(&|dir| Ok(fs::write(dir.join("notes.txt"), "")?))?);
    assert!(!copy(&|_| Ok(()))?, "an untouched copy passes");
    Ok(())
  }
  #[test]
  fn maintenance_cannot_disable_shutdown_or_restart_the_removed_app() -> Result<()> {
    for (xml, element, name) in [
      (
        include_str!("../../../windows/installer/Package.wxs"),
        "Property",
        "Id",
      ),
      (
        include_str!("../../../windows/installer/Bundle.wxs"),
        "MsiProperty",
        "Name",
      ),
    ] {
      installer_shutdown_contract(xml, element, name)?;
      for (property, old, invalid) in [
        ("MSIRESTARTMANAGERCONTROL", "0", "Disable"),
        ("MSIRMSHUTDOWN", "1", "2"),
        ("MSIDISABLERMRESTART", "1", "0"),
      ] {
        let setting = format!(r#"<{element} {name}="{property}" Value="{old}" />"#);
        assert!(installer_shutdown_contract(&xml.replace(&setting, ""), element, name).is_err());
        let broken = xml.replace(
          &setting,
          &format!(r#"<{element} {name}="{property}" Value="{invalid}" />"#),
        );
        assert!(
          installer_shutdown_contract(&broken, element, name).is_err(),
          "{property}={invalid}"
        );
      }
    }
    Ok(())
  }
}
