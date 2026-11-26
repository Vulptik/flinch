use crate::{
  checks, packaging,
  process::{self, cargo},
  read,
};
use anyhow::{Context, Result, bail, ensure};
use std::{collections::BTreeSet, fs};

pub const COMMANDS: &[&str] = &[
  "help",
  "doctor",
  "test",
  "test:run",
  "test:tooling",
  "test:blur",
  "test:smallpeg",
  "test:video",
  "test:video:gpu",
  "test:video:mf",
  "test:video:vendor",
  "test:video:vendor:rehearsal",
  "test:video:smallest",
  "test:video:dxgi:desktop",
  "test:coverage",
  "test:speed:desktop",
  "speed:ratchet",
  "test:recording",
  "test:recording:desktop",
  "test:capture:desktop",
  "test:clipboard:desktop",
  "test:recording:sync:desktop",
  "test:settings",
  "test:settings:desktop",
  "check:settings:coverage",
  "check:settings",
  "check:docs",
  "check:contracts",
  "check:quick",
  "check:all",
  "check:release",
  "check:format",
  "check:clippy",
  "test:installer",
  "test:installer:startup",
  "test:installer:plan",
  "test:installer:ui",
  "pack:msix",
  "pack:installer",
  "pack:vendor-kit",
  "installer:tools",
  "version:sync",
  "assets:app",
  "assets:store",
  "assets:verify",
  "assets:verify-executable",
  "installer:verify-imports",
  "installer:validate-runtime",
];
pub const QUIET: &[&str] = &[
  "compositor_core",
  "config_defaults_tests",
  "config_operations",
  "config_validation_test",
  "monitor_math_tests",
  "monitors_in_rect",
  "screenshot_integration",
  "shortcut_normalization_tests",
  "video_config_tests",
];
/// The production worker the native pipeline tests launch (`target/debug/flinch.exe`).
const RECORDING_WORKER_BUILD: &[&str] = &["build", "--bin", "flinch"];
/// The recorder's tests against a real worker with synthetic sources (also measured by coverage).
pub const NATIVE_RECORDING_FILTER: &str = "video::controller::native_pipeline_tests::";
/// The native recorder end to end against the real worker, with synthetic sources.
const NATIVE_RECORDING_PLAN: &[&str] = &[
  "test",
  "--no-default-features",
  "--features",
  "native-settings",
  "--lib",
  NATIVE_RECORDING_FILTER,
  "--",
  "--ignored",
  "--test-threads=1",
];
/// The native recorder against the real desktop and loopback audio.
const NATIVE_DESKTOP_RECORDING_PLAN: &[&str] = &[
  "test",
  "--no-default-features",
  "--features",
  "native-settings",
  "--lib",
  "video::controller::native_desktop_tests::",
  "--",
  "--ignored",
  "--test-threads=1",
];

pub fn help() -> Result<()> {
  println!(
    "cargo xtask <task> [options]\n{}\nDesktop tasks require an explicit desktop session. Packaging never installs or publishes.",
    COMMANDS.join("\n")
  );
  Ok(())
}

pub fn test_plan(options: &[String]) -> Result<Vec<String>> {
  let allowed = ["--clipboard", "--integration-only", "--list"];
  ensure!(
    options.iter().all(|s| allowed.contains(&s.as_str()))
      && options.iter().collect::<BTreeSet<_>>().len() == options.len(),
    "Unknown or duplicate test option"
  );
  let modes: Vec<_> = options.iter().filter(|s| *s != "--list").collect();
  ensure!(modes.len() <= 1, "Select one test mode");
  let mut args: Vec<String> = [
    "test",
    "--manifest-path",
    "Cargo.toml",
    "--no-default-features",
    "--features",
    "native-settings",
  ]
  .map(String::from)
  .to_vec();
  match modes.first().map(|s| s.as_str()) {
    Some("--clipboard") => {
      args.extend(["--lib", "commands::clipboard::tests::", "--", "--ignored"].map(String::from))
    },
    mode => {
      if mode != Some("--integration-only") {
        args.push("--lib".into());
      }
      for suite in QUIET {
        args.extend(["--test".into(), (*suite).into()]);
      }
      args.push("--".into());
    },
  }
  args.extend(["--test-threads=1".into(), "--quiet".into()]);
  Ok(args)
}

pub fn inventory(names: &[String]) -> Result<()> {
  let expected: BTreeSet<_> = QUIET.iter().map(|s| (*s).to_owned()).collect();
  let actual: BTreeSet<_> = names.iter().cloned().collect();
  ensure!(
    expected == actual,
    "Integration inventory differs: unclassified={:?}, missing={:?}. Update crates/xtask/src/tasks.rs.",
    actual.difference(&expected).collect::<Vec<_>>(),
    expected.difference(&actual).collect::<Vec<_>>()
  );
  Ok(())
}

fn tests(options: &[String]) -> Result<()> {
  let plan = test_plan(options)?;
  let names = fs::read_dir("tests")?
    .map(|entry| -> Result<Option<String>> {
      let path = entry?.path();
      Ok(if path.extension().is_some_and(|s| s == "rs") {
        Some(
          path
            .file_stem()
            .context("Test name")?
            .to_string_lossy()
            .into_owned(),
        )
      } else {
        None
      })
    })
    .collect::<Result<Vec<_>>>()?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
  inventory(&names)?;
  println!("cargo {}", plan.join(" "));
  if options.iter().any(|s| s == "--list") {
    return Ok(());
  }
  process::run(process::command("cargo").args(plan), 1800).map(|_| ())
}

/// A self-contained folder for an NVIDIA/Intel machine without Rust or Visual Studio:
/// the lab, the engine test binary, the VC runtime and `run-vendor-kit.bat`. Never installed.
fn vendor_kit() -> Result<()> {
  cargo(&["build", "--release", "-p", "flinch-video-lab"])?;
  let messages = process::capture(
    process::command("cargo").args([
      "test",
      "--release",
      "-p",
      "flinch-video-engine",
      "--lib",
      "--no-run",
      "--message-format=json",
    ]),
    1800,
  )?;
  let tests = messages
    .lines()
    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    .filter(|message| message["target"]["name"] == "flinch_video_engine")
    .find_map(|message| message["executable"].as_str().map(str::to_owned))
    .context("Cargo reported no engine test executable")?;
  let kit = std::path::Path::new("target/vendor-kit");
  fs::create_dir_all(kit)?;
  let system = std::env::var("SystemRoot").context("SystemRoot is not set")?;
  let copies = [
    (
      "target/release/flinch-video-lab.exe".to_owned(),
      "flinch-video-lab.exe",
    ),
    (tests, "engine-tests.exe"),
    (
      "crates/video-lab/vendor-kit/run-vendor-kit.bat".to_owned(),
      "run-vendor-kit.bat",
    ),
    // Machines without the Visual C++ Redistributable still run the kit.
    (
      format!("{system}/System32/vcruntime140.dll"),
      "vcruntime140.dll",
    ),
    (
      format!("{system}/System32/vcruntime140_1.dll"),
      "vcruntime140_1.dll",
    ),
  ];
  for (from, to) in copies {
    fs::copy(&from, kit.join(to)).with_context(|| format!("Copy {from}"))?;
  }
  println!("Vendor kit: {}", kit.display());
  Ok(())
}

/// Offscreen NVENC/QSV acceptance on NVIDIA or Intel hardware.
/// The rehearsal runs the same harness through AMF on AMD hardware.
fn video_vendor(rehearsal: bool) -> Result<()> {
  let mut command = process::command("cargo");
  command.args([
    "test",
    "--release",
    "-p",
    "flinch-video-engine",
    "--lib",
    "windows::vendor_acceptance::vendor_encoder_offscreen_acceptance",
    "--",
    "--ignored",
    "--exact",
    "--nocapture",
    "--test-threads=1",
  ]);
  if rehearsal {
    command.env("FLINCH_VENDOR_REHEARSAL", "1");
  }
  process::run(&mut command, 1800).map(|_| ())
}
fn video_gpu() -> Result<()> {
  cargo(&[
    "test",
    "-p",
    "flinch-video-engine",
    "--lib",
    "windows::cursor::pixel_tests::offscreen_cursor_and_bt709_pixels",
    "--",
    "--ignored",
    "--exact",
    "--test-threads=1",
  ])?;
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)?
    .as_nanos();
  let output = format!("target/video-lab-probes/{nonce}/synthetic.mp4");
  cargo(&[
    "run",
    "-p",
    "flinch-video-lab",
    "--",
    "scenario",
    "--capture",
    "synthetic",
    "--audio",
    "--no-cursor",
    "--output",
    &output,
    "--duration",
    "8",
  ])?;
  cargo(&[
    "run",
    "-p",
    "flinch-video-lab",
    "--",
    "verify",
    &output,
    "--markers",
  ])
}

pub fn dispatch(task: &str, options: &[String]) -> Result<()> {
  if ![
    "test",
    "test:run",
    "pack:msix",
    "version:sync",
    "assets:verify-executable",
    "installer:verify-imports",
  ]
  .contains(&task)
  {
    ensure!(options.is_empty(), "{task} accepts no options");
  }
  match task {
    "help" => help(),
    "doctor" => doctor(),
    "test" | "test:run" => tests(options),
    "test:tooling" => cargo(&["test", "-p", "xtask", "--bin", "xtask"]),
    "test:blur" => cargo(&["test", "-p", "flinch-blur", "--", "--test-threads=1"]),
    // The vendored JPG encoder with Flinch's threading; see vendor/smallpeg/FLINCH-PATCH.md.
    // Outside the workspace (patched in), so it builds from its own manifest and upstream lockfile.
    "test:smallpeg" => cargo(&[
      "test",
      "--release",
      "--locked",
      "--manifest-path",
      "vendor/smallpeg/Cargo.toml",
      "--target-dir",
      "target",
      "--features",
      "parallel",
      "--lib",
    ]),
    "test:video" => cargo(&[
      "test",
      "-p",
      "flinch-video-engine",
      "-p",
      "flinch-video-lab",
      "--lib",
      "--",
      "--test-threads=1",
    ]),
    "test:video:gpu" => video_gpu(),
    "test:video:dxgi:desktop" => cargo(&[
      "test",
      "-p",
      "flinch-video-engine",
      "--lib",
      "windows::capture::desktop_tests::desktop_duplication_initialization_diagnostic",
      "--",
      "--ignored",
      "--exact",
      "--nocapture",
      "--test-threads=1",
    ]),
    "test:video:vendor" => video_vendor(false),
    "test:video:vendor:rehearsal" => video_vendor(true),
    // Offscreen, but drives every encoder on this machine to find its smallest recordable size.
    "test:video:smallest" => cargo(&[
      "test",
      "-p",
      "flinch-video-engine",
      "--lib",
      "windows::vendor_acceptance::smallest_area_probe",
      "--",
      "--ignored",
      "--exact",
      "--nocapture",
      "--test-threads=1",
    ]),
    "test:video:mf" => cargo(&[
      "test",
      "-p",
      "flinch-video-engine",
      "--lib",
      "hardware_reference_cpu_input_diagnostic",
      "--",
      "--ignored",
      "--nocapture",
    ]),
    "test:coverage" => crate::coverage::run(),
    "test:speed:desktop" => crate::speed::run(false),
    "speed:ratchet" => crate::speed::run(true),
    "test:recording" => {
      cargo(&[
        "test",
        "--no-default-features",
        "--features",
        "native-settings",
        "--lib",
        "video::controller",
        "--",
        "--test-threads=1",
      ])?;
      cargo(RECORDING_WORKER_BUILD)?;
      cargo(NATIVE_RECORDING_PLAN)
    },
    "test:recording:desktop" => {
      cargo(RECORDING_WORKER_BUILD)?;
      cargo(NATIVE_DESKTOP_RECORDING_PLAN)
    },
    "test:capture:desktop" => {
      cargo(&[
        "test",
        "--no-default-features",
        "--features",
        "native-settings",
        "--lib",
        "test_try_gdi_capture_for_video_returns_frame",
        "--",
        "--ignored",
        "--test-threads=1",
      ])?;
      cargo(&[
        "test",
        "--no-default-features",
        "--features",
        "native-settings",
        "--lib",
        "screenshot::platform::windows::wgc::tests::",
        "--",
        "--ignored",
        "--test-threads=1",
      ])?;
      cargo(&[
        "test",
        "--no-default-features",
        "--features",
        "native-settings",
        "--lib",
        "desktop_dxgi_cold_repeat_and_trimmed_capture",
        "--",
        "--ignored",
        "--test-threads=1",
      ])?;
      cargo(&[
        "test",
        "--no-default-features",
        "--features",
        "native-settings",
        "--lib",
        "native_field_reads_unicode_and_posts_cancel_without_orphaning_control",
        "--",
        "--ignored",
        "--test-threads=1",
      ])
    },
    "test:clipboard:desktop" => tests(&["--clipboard".into()]),
    "test:recording:sync:desktop" => process::powershell("scripts/recording-sync-probe.ps1", &[]),
    "check:format" => cargo(&["fmt", "--all", "--check"]),
    "check:clippy" => cargo(&[
      "clippy",
      "--workspace",
      "--all-targets",
      "--",
      "-D",
      "warnings",
    ]),
    "check:settings" => {
      dispatch("check:format", &[])?;
      dispatch("check:clippy", &[])
    },
    "check:docs" => checks::docs(),
    "check:contracts" => checks::contracts(),
    "check:settings:coverage" => checks::settings_coverage(),
    "test:settings" => settings(false),
    "test:settings:desktop" => settings(true),
    "check:quick" | "check:all" => {
      for step in [
        "check:docs",
        "check:contracts",
        "test:tooling",
        "test:video",
        "test:blur",
        "test:smallpeg",
        "check:settings:coverage",
        "check:format",
      ] {
        dispatch(step, &[])?;
      }
      if task == "check:all" {
        dispatch("check:clippy", &[])?;
        // The installer compiles app files through #[path]; only its own build sees breaks there.
        dispatch("test:installer", &[])?;
        settings(false)?;
      }
      Ok(())
    },
    "check:release" => {
      for step in [
        "check:all",
        "test:coverage",
        "test",
        "test:recording",
        "test:settings:desktop",
        "test:recording:desktop",
        "test:clipboard:desktop",
        "test:speed:desktop",
      ] {
        dispatch(step, &[])?;
      }
      Ok(())
    },
    "test:installer" => {
      cargo(&[
        "test",
        "-p",
        "flinch-installer",
        "--features",
        "native-instance",
        "--lib",
      ])?;
      checks::installer()
    },
    "installer:tools" => process::powershell("scripts/setup-installer-tools.ps1", &[]),
    "test:installer:ui" => process::powershell("scripts/check-installer-ui.ps1", &[]),
    "pack:installer" | "test:installer:plan" | "test:installer:startup" => {
      packaging::installer(task)
    },
    "pack:vendor-kit" => vendor_kit(),
    "pack:msix" => packaging::msix(options),
    "version:sync" => packaging::sync_version(options),
    "installer:validate-runtime" => packaging::validate_runtime(&serde_json::from_str(
      &std::env::var("FLINCH_REDIST_METADATA")?,
    )?),
    "assets:verify-executable" | "installer:verify-imports" => {
      ensure!(options.len() == 1, "Supply one executable path");
      if task == "assets:verify-executable" {
        crate::pe::branding(std::path::Path::new(&options[0]))
      } else {
        crate::pe::imports(std::path::Path::new(&options[0]))
      }
    },
    "assets:app" | "assets:store" | "assets:verify" => crate::assets::run(task),
    _ => bail!("Unknown task {task}"),
  }
}

fn doctor() -> Result<()> {
  let mut failures = Vec::new();
  ensure!(cfg!(windows), "Windows required");
  for (tool, args) in [
    ("git", vec!["--version"]),
    ("cargo", vec!["--version"]),
    ("rustc", vec!["-vV"]),
    ("rustfmt", vec!["--version"]),
    ("cargo", vec!["clippy", "--version"]),
  ] {
    match process::run(process::command(tool).args(args), 10) {
      Ok(out) if tool == "rustc" && !out.contains("windows-msvc") => {
        failures.push("Rust MSVC host required".to_owned())
      },
      Err(e) => failures.push(format!("{e:#}")),
      _ => (),
    }
  }
  ensure!(
    failures.is_empty(),
    "Prerequisite failures:\n{}",
    failures.join("\n")
  );
  println!(
    "Prerequisites passed. No downloads/builds/app launches performed by doctor; C++ SDK linking remains a build gate."
  );
  Ok(())
}

fn settings(desktop: bool) -> Result<()> {
  checks::settings_coverage()?;
  // Include the application to share its Slint/Skia feature set and cache.
  cargo(&[
    "test",
    "-p",
    "i-slint-renderer-skia",
    "-p",
    "flinch",
    "--no-default-features",
    "--features",
    "native-settings",
    "--lib",
    "windows_text::",
    "--",
    "--test-threads=1",
  ])?;
  cargo(&[
    "test",
    "-p",
    "flinch-settings",
    "--features",
    "ui",
    "--lib",
    "--",
    "--test-threads=1",
  ])?;
  cargo(&[
    "test",
    "--no-default-features",
    "--features",
    "native-settings",
    "--lib",
    "settings::",
    "--",
    "--test-threads=1",
  ])?;
  if !desktop {
    return Ok(());
  }
  // Unplugs a temporary drive letter under a custom save folder.
  cargo(&[
    "test",
    "--no-default-features",
    "--features",
    "native-settings",
    "--lib",
    "commands::screenshot::post_capture::drive_tests::",
    "--",
    "--ignored",
    "--test-threads=1",
  ])?;
  cargo(&[
    "build",
    "-p",
    "flinch-settings",
    "--features",
    "ui",
    "--example",
    "settings-probe",
  ])?;
  let out = process::run(
    process::command("target/debug/examples/settings-probe.exe").arg("--check"),
    120,
  )?;
  ensure!(
    regex::Regex::new(
      r"Native settings: [1-9]\d* production UI interactions passed across five locales\."
    )?
    .is_match(&out),
    "Incomplete UI interaction matrix"
  );
  let root = std::env::current_dir()?
    .join("artifacts/performance/settings-checks")
    .join(packaging::unique());
  fs::create_dir_all(&root)?;
  let result = (|| -> Result<()> {
    for (probe, name, timeout) in [
      ("--probe-polish", "polish", 15),
      ("--probe-reset-dialog", "reset-dialog", 60),
      ("--probe-discard-dialog", "discard-dialog", 60),
    ] {
      process::run(
        process::command("target/debug/examples/settings-probe.exe")
          .arg(probe)
          .arg(root.join(name)),
        timeout,
      )?;
    }
    for channel in ["installer", "msix"] {
      // Disable defaults explicitly so the MSIX feature cannot leak into installer probes.
      let features = if channel == "msix" {
        "native-settings,desktop-app,msix"
      } else {
        "native-settings,desktop-app"
      };
      cargo(&["build", "--no-default-features", "--features", features])?;
      for suffix in ["", "-quit-failure", "-load-failure"] {
        let output = root.join(format!("{channel}{suffix}"));
        let mut cmd = process::command("target/debug/flinch.exe");
        cmd.arg("--native-settings-smoke").arg(&output);
        if !suffix.is_empty() {
          cmd.arg(format!("--native-settings-smoke{suffix}"));
        }
        process::run(&mut cmd, 60)?;
        if output.join("failure.txt").exists() {
          bail!("{}", read(output.join("failure.txt"))?);
        }
        let report = crate::json(output.join("results.json"))?;
        let keys: &[&str] = match suffix {
          "-quit-failure" => &[
            "explicit_quit_discards_invalid_without_prompt",
            "explicit_quit_discards_failed_write",
          ],
          "-load-failure" => &["startup_failure_keeps_other_settings_editable"],
          _ => &[
            "explicit_quit_discards_invalid_without_prompt",
            "invalid_close_preserves_valid_edits",
            "inline_template_error_and_focus",
            "failed_save_preserved_config",
            "retry_persisted_both_fields",
            "shortcut_retry_persisted",
            "capture_close_restored",
            "action_close_and_cancel",
            "template_resets_persisted",
            "save_location_persisted_and_restored",
            "update_preference_persisted",
            "preview_waited_for_save",
            "cooldown_survived_navigation_and_reopen",
            "configuration_actions_refresh_and_failures",
          ],
        };
        for key in keys {
          ensure!(report[key] == true, "{channel}{suffix}: missing {key}");
        }
        if suffix.is_empty() {
          ensure!(
            report["reopen_cycles"] == 20
              && report["mock_manifest_reads"] == u64::from(channel == "installer"),
            "Lifecycle/update ownership failed"
          );
        }
      }
    }
    Ok(())
  })();
  let restore = cargo(&["build"]);
  match (result, restore) {
    (Err(e), Err(r)) => bail!("{e:#}; restoring installer build also failed: {r:#}"),
    (Err(e), _) | (_, Err(e)) => Err(e),
    _ => Ok(()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn quiet_never_selects_devices_or_gui() -> Result<()> {
    for options in [
      vec![],
      vec!["--integration-only".into()],
      vec!["--list".into()],
    ] {
      let plan = test_plan(&options)?;
      assert!(plan.contains(&"--no-default-features".into()));
      assert!(!plan.contains(&"--ignored".into()));
      for suite in QUIET {
        assert!(plan.contains(&(*suite).into()));
      }
    }
    Ok(())
  }
  #[test]
  fn modes_fail_closed() {
    for args in [
      vec!["--ignored"],
      vec!["--recording"],
      vec!["--list", "--list"],
      vec!["--clipboard", "--integration-only"],
    ] {
      assert!(test_plan(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
    }
  }
  #[test]
  fn native_recording_plan_selects_the_worker_suite() -> Result<()> {
    let quick = NATIVE_RECORDING_PLAN
      .iter()
      .position(|arg| *arg == "--")
      .context("Missing separator")?;
    assert_eq!(
      NATIVE_RECORDING_PLAN[quick - 1],
      "video::controller::native_pipeline_tests::"
    );
    let filters = &NATIVE_RECORDING_PLAN[quick + 1..];
    assert!(filters.contains(&"--ignored") && filters.contains(&"--test-threads=1"));
    assert_eq!(RECORDING_WORKER_BUILD, ["build", "--bin", "flinch"]);
    Ok(())
  }
  #[test]
  fn native_desktop_plan_selects_only_the_desktop_suite() -> Result<()> {
    let separator = NATIVE_DESKTOP_RECORDING_PLAN
      .iter()
      .position(|arg| *arg == "--")
      .context("Missing separator")?;
    assert_eq!(
      NATIVE_DESKTOP_RECORDING_PLAN[separator - 1],
      "video::controller::native_desktop_tests::"
    );
    let filters = &NATIVE_DESKTOP_RECORDING_PLAN[separator + 1..];
    assert!(filters.contains(&"--ignored") && filters.contains(&"--test-threads=1"));
    assert!(NATIVE_DESKTOP_RECORDING_PLAN.contains(&"--no-default-features"));
    Ok(())
  }
  #[test]
  fn desktop_scoping() -> Result<()> {
    let clipboard = test_plan(&["--clipboard".into()])?;
    assert!(clipboard.contains(&"commands::clipboard::tests::".into()));
    assert!(!clipboard.contains(&"--test".into()));
    Ok(())
  }
  #[test]
  fn inventory_rejects_new_and_missing_targets() {
    let mut names: Vec<_> = QUIET.iter().map(|s| (*s).to_owned()).collect();
    assert!(inventory(&names).is_ok());
    names.push("new_capture".into());
    assert!(inventory(&names).is_err());
    names.pop();
    names.pop();
    assert!(inventory(&names).is_err());
  }
}
