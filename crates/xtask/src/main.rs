mod assets;
mod checks;
mod coverage;
mod packaging;
mod pe;
mod process;
mod speed;
mod tasks;

use anyhow::{Context, Result, bail};
use std::{env, fs, path::Path};

pub fn read(path: impl AsRef<Path>) -> Result<String> {
  let path = path.as_ref();
  fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))
}

pub fn json(path: impl AsRef<Path>) -> Result<serde_json::Value> {
  serde_json::from_str(&read(path)?).context("Parse JSON")
}

pub fn version() -> Result<String> {
  let cargo = read("Cargo.toml")?;
  let re = regex::Regex::new(r#"(?m)^version\s*=\s*"([^"]+)""#)?;
  Ok(re.captures(&cargo).context("Missing app version")?[1].to_owned())
}

fn main() {
  if let Err(error) = main_result() {
    eprintln!("{error:#}");
    std::process::exit(1);
  }
}

fn main_result() -> Result<()> {
  let args: Vec<String> = env::args().skip(1).collect();
  // Before `execute` changes directory: rustc resolves its relative paths from Cargo's cwd.
  if env::var_os(coverage::WRAPPER_ENV).is_some() {
    std::process::exit(coverage::rustc_wrapper(&args)?);
  }
  execute(&args)
}

fn execute(args: &[String]) -> Result<()> {
  env::set_current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))?;
  let Some((task, options)) = args.split_first() else {
    return tasks::help();
  };
  if !tasks::COMMANDS.contains(&task.as_str()) {
    bail!("Unknown task: {task}. Run cargo xtask help.");
  }
  tasks::dispatch(task, options)
}

#[cfg(test)]
mod cli_tests {
  use super::*;
  #[test]
  fn child_entry() -> Result<()> {
    let Ok(args) = env::var("FLINCH_XTASK_TEST_ARGS") else {
      return Ok(());
    };
    let args: Vec<String> = serde_json::from_str(&args)?;
    execute(&args)
  }
  #[test]
  fn dry_run_is_independent_of_working_directory_and_cargo() -> Result<()> {
    let temp = tempfile::tempdir()?;
    for mode in [None, Some("--clipboard"), Some("--integration-only")] {
      let mut args = vec!["test", "--list"];
      if let Some(mode) = mode {
        args.push(mode);
      }
      let output = process::capture(
        process::command(env::current_exe()?)
          .args(["--exact", "cli_tests::child_entry", "--nocapture"])
          .current_dir(temp.path())
          .env_remove("PATH")
          .env("FLINCH_XTASK_TEST_ARGS", serde_json::to_string(&args)?),
        10,
      )?;
      anyhow::ensure!(
        output.contains("--no-default-features --features native-settings"),
        "Missing quiet build flags"
      );
    }
    Ok(())
  }
  #[test]
  fn invalid_options_fail_before_spawning() -> Result<()> {
    for args in [
      vec!["test", "--ignored"],
      vec!["test", "--clipboard", "--integration-only"],
      vec!["pack:msix", "--skip-build"],
      vec!["check:quick", "--desktop"],
      vec!["test:coverage", "--skip-tests"],
      vec!["test:coverage", "--minimum", "0"],
      vec!["test:speed:desktop", "--runs", "5"],
      vec!["speed:ratchet", "--force"],
      vec!["unknown-task"],
    ] {
      assert!(
        process::capture(
          process::command(env::current_exe()?)
            .args(["--exact", "cli_tests::child_entry", "--nocapture"])
            .env_remove("PATH")
            .env("FLINCH_XTASK_TEST_ARGS", serde_json::to_string(&args)?),
          10
        )
        .is_err()
      );
    }
    Ok(())
  }
}
