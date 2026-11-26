/// Intel's oneVPL dispatcher sources (unmodified, `native/vpl/dispatcher/src`), as listed by
/// libvpl 2.17.0's own CMakeLists.txt for Windows.
const VPL_DISPATCHER: [&str; 18] = [
  "windows/main.cpp",
  "windows/mfx_critical_section.cpp",
  "windows/mfx_dispatcher.cpp",
  "windows/mfx_dispatcher_log.cpp",
  "windows/mfx_driver_store_loader.cpp",
  "windows/mfx_dxva2_device.cpp",
  "windows/mfx_function_table.cpp",
  "windows/mfx_library_iterator.cpp",
  "windows/mfx_load_dll.cpp",
  "windows/mfx_win_reg_key.cpp",
  "mfx_dispatcher_vpl.cpp",
  "mfx_dispatcher_vpl_loader.cpp",
  "mfx_dispatcher_vpl_config.cpp",
  "mfx_dispatcher_vpl_lowlatency.cpp",
  "mfx_dispatcher_vpl_log.cpp",
  "mfx_dispatcher_vpl_msdk.cpp",
  "mfx_config_interface/mfx_config_interface.cpp",
  "mfx_config_interface/mfx_config_interface_string_api.cpp",
];

fn main() {
  for path in [
    "native/amf_bridge.cpp",
    "native/amf",
    "native/nvenc_bridge.c",
    "native/nvenc",
    "native/qsv_bridge.cpp",
    "native/vpl",
  ] {
    println!("cargo:rerun-if-changed={path}");
  }
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
    cc::Build::new()
      .cpp(true)
      .std("c++17")
      .file("native/amf_bridge.cpp")
      .include("native")
      .define("WIN32_LEAN_AND_MEAN", None)
      .define("NOMINMAX", None)
      .flag_if_supported("/EHsc")
      .compile("flinch_amf_bridge");
    cc::Build::new()
      .file("native/nvenc_bridge.c")
      .include("native")
      .define("WIN32_LEAN_AND_MEAN", None)
      .define("NOMINMAX", None)
      .compile("flinch_nvenc_bridge");
    let mut vpl = cc::Build::new();
    vpl
      .cpp(true)
      .std("c++17")
      .file("native/qsv_bridge.cpp")
      .files(
        VPL_DISPATCHER
          .iter()
          .map(|file| format!("native/vpl/dispatcher/src/{file}")),
      )
      .include("native/vpl")
      .include("native/vpl/dispatcher")
      .define("MFX_DEPRECATED_OFF", None)
      .define("NOMINMAX", None)
      .define("UNICODE", None)
      .define("_UNICODE", None)
      // Vendor sources are compiled as published; their warnings are not Flinch's to fix.
      .warnings(false)
      .flag_if_supported("/EHsc");
    vpl.compile("flinch_qsv_bridge");
    for library in [
      "advapi32", "ole32", "dxgi", "d3d9", "cfgmgr32", "setupapi", "gdi32",
    ] {
      println!("cargo:rustc-link-lib={library}");
    }
  }
}
