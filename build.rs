use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  println!("cargo:rerun-if-changed=windows/app.manifest");
  println!("cargo:rerun-if-changed=icons/icon.ico");

  if cfg!(target_os = "windows") {
    let manifest_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?)
      .join("windows")
      .join("app.manifest");

    // Ensure every Windows executable produced from this crate, including Rust test
    // harnesses in target/*/deps, activates Common Controls v6.
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
    println!(
      "cargo:rustc-link-arg=/MANIFESTINPUT:{}",
      manifest_path.display()
    );
  }

  if cfg!(target_os = "windows") {
    winresource::WindowsResource::new()
      .set_icon("icons/icon.ico")
      .set("ProductName", "Flinch")
      .set("FileDescription", "Flinch")
      .compile()?;
  }
  Ok(())
}
