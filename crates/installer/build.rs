fn main() {
  #[cfg(feature = "ui")]
  {
    if let Err(error) = slint_build::compile("ui/installer.slint") {
      eprintln!("Installer UI compilation failed: {error}");
      std::process::exit(1);
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let icon = root.join("icons/icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if let Err(error) = winresource::WindowsResource::new()
      .set_icon(&icon.to_string_lossy())
      .set("ProductName", "Flinch")
      .set("FileDescription", "Flinch Setup")
      .set("OriginalFilename", "flinch-installer.exe")
      .compile()
    {
      eprintln!("Installer Windows branding compilation failed: {error}");
      std::process::exit(1);
    }
    let native = std::env::var_os("FLINCH_BURN_BRIDGE_DIR")
      .map(std::path::PathBuf::from)
      .unwrap_or_else(|| root.join(".tools/installer/native"));
    println!("cargo:rustc-link-search=native={}", native.display());
    println!("cargo:rustc-link-lib=dylib=flinch_burn");
    println!("cargo:rerun-if-env-changed=FLINCH_BURN_BRIDGE_DIR");
  }
}
