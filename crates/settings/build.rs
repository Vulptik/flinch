fn main() -> Result<(), Box<dyn std::error::Error>> {
  #[cfg(feature = "ui")]
  build_ui()?;
  Ok(())
}

#[cfg(feature = "ui")]
fn build_ui() -> Result<(), Box<dyn std::error::Error>> {
  use std::{env, fs, path::PathBuf};
  let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
  let output = PathBuf::from(env::var("OUT_DIR")?);
  let repo = root.join("../..");
  let locales = ["en", "fr", "de", "es", "it"];
  let mut catalogs = Vec::new();
  for locale in locales {
    let path = repo.join("messages").join(format!("{locale}.json"));
    println!("cargo:rerun-if-changed={}", path.display());
    catalogs.push(serde_json::from_str::<
      serde_json::Map<String, serde_json::Value>,
    >(&fs::read_to_string(path)?)?);
  }
  let mut copy = String::from(
    "export global Copy {\n    in-out property <int> language: 0;\n    out property <string> version: \"",
  );
  copy.push_str(env!("CARGO_PKG_VERSION"));
  copy.push_str("\";\n");
  for key in catalogs[0].keys() {
    let values = catalogs
      .iter()
      .map(|catalog| {
        catalog
          .get(key)
          .and_then(serde_json::Value::as_str)
          .ok_or_else(|| format!("Missing string translation: {key}"))
          .and_then(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
      })
      .collect::<Result<Vec<_>, _>>()?;
    copy.push_str(&format!(
      "    out property <string> {key}: [{}][language];\n",
      values.join(", ")
    ));
  }
  for key in [
    "sidebar_general",
    "sidebar_notifications",
    "sidebar_hotkeys",
    "sidebar_screenshots",
    "sidebar_videos",
    "sidebar_files",
    "sidebar_tools",
    "system_configuration",
    "screenshots_gif_recording",
    "hotkeys_screenshots",
    "hotkeys_videos",
    "about_support",
    "about_diagnostics",
    "about_more",
    "system_save_location",
    "system_single_folder_subfolder_template_preview",
  ] {
    let values = catalogs
      .iter()
      .map(|catalog| {
        let value = catalog
          .get(key)
          .and_then(serde_json::Value::as_str)
          .ok_or_else(|| format!("Missing heading: {key}"))?;
        Ok(serde_json::to_string(&value.to_uppercase())?)
      })
      .collect::<Result<Vec<String>, Box<dyn std::error::Error>>>()?;
    copy.push_str(&format!(
      "    out property <string> {key}_upper: [{}][language];\n",
      values.join(", ")
    ));
  }
  copy.push_str("}\n");
  fs::write(output.join("copy.slint"), copy)?;
  let config = slint_build::CompilerConfiguration::new()
    .with_style("fluent".into())
    .with_include_paths(vec![output]);
  slint_build::compile_with_config(root.join("ui/settings.slint"), config)?;
  #[cfg(windows)]
  {
    println!("cargo:rerun-if-changed=../../windows/app.manifest");
    println!("cargo:rerun-if-changed=../../icons/icon.ico");
    winresource::WindowsResource::new()
      .set_icon("../../icons/icon.ico")
      .set_manifest_file("../../windows/app.manifest")
      .compile()?;
  }
  Ok(())
}
