//! Bounded subprocesses. Pipe readers run concurrently so neither stream can block a child.
use anyhow::{Context, Result, bail};
use std::{
  io::Read,
  process::{Command, Stdio},
  sync::mpsc,
  thread,
  time::{Duration, Instant},
};

pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
  let mut cmd = Command::new(program);
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
  }
  cmd
}

pub fn run(cmd: &mut Command, seconds: u64) -> Result<String> {
  execute(cmd, seconds, true)
}

pub fn capture(cmd: &mut Command, seconds: u64) -> Result<String> {
  execute(cmd, seconds, false)
}

fn execute(cmd: &mut Command, seconds: u64, echo: bool) -> Result<String> {
  if echo {
    println!("> {cmd:?}");
  }
  let mut child = cmd
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .with_context(|| format!("Start {cmd:?}"))?;
  let stdout = child.stdout.take().context("Missing child stdout")?;
  let stderr = child.stderr.take().context("Missing child stderr")?;
  let drain = |mut pipe: Box<dyn Read + Send>| -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    // Bounded retained output; drain the remainder to avoid pipe deadlocks.
    pipe
      .by_ref()
      .take(16 * 1024 * 1024)
      .read_to_end(&mut output)?;
    let overflow = std::io::copy(&mut pipe, &mut std::io::sink())?;
    if overflow > 0 {
      return Err(std::io::Error::other(
        "Subprocess output exceeded 16 MiB retention limit",
      ));
    }
    Ok(output)
  };
  let (out_tx, out_rx) = mpsc::sync_channel(1);
  let (err_tx, err_rx) = mpsc::sync_channel(1);
  thread::spawn(move || {
    if out_tx.send(drain(Box::new(stdout))).is_err() {
      eprintln!("stdout receiver closed after subprocess deadline");
    }
  });
  thread::spawn(move || {
    if err_tx.send(drain(Box::new(stderr))).is_err() {
      eprintln!("stderr receiver closed after subprocess deadline");
    }
  });
  let deadline = Instant::now() + Duration::from_secs(seconds);
  let status = loop {
    if let Some(status) = child.try_wait()? {
      break Some(status);
    }
    if Instant::now() >= deadline {
      #[cfg(windows)]
      {
        let result = command("taskkill.exe")
          .args(["/PID", &child.id().to_string(), "/T", "/F"])
          .stdout(Stdio::null())
          .stderr(Stdio::null())
          .spawn();
        match result {
          Ok(mut killer) => {
            let kill_deadline = Instant::now() + Duration::from_secs(10);
            loop {
              if killer.try_wait()?.is_some() {
                break;
              }
              if Instant::now() >= kill_deadline {
                killer.kill().context("Terminate overdue taskkill")?;
                break;
              }
              thread::sleep(Duration::from_millis(20));
            }
          },
          Err(error) => eprintln!("Child tree termination failed: {error}"),
        }
      }
      if let Err(error) = child.kill() {
        eprintln!("Child termination: {error}");
      }
      let reap_deadline = Instant::now() + Duration::from_secs(5);
      while child.try_wait()?.is_none() {
        if Instant::now() >= reap_deadline {
          bail!("Timed-out child could not be reaped: {cmd:?}");
        }
        thread::sleep(Duration::from_millis(20));
      }
      break None;
    }
    thread::sleep(Duration::from_millis(20));
  };
  // Descendants retaining inherited pipes cannot turn exit/timeout into an unbounded join.
  let stdout = out_rx
    .recv_timeout(Duration::from_secs(2))
    .context("Child stdout did not close")??;
  let stderr = err_rx
    .recv_timeout(Duration::from_secs(2))
    .context("Child stderr did not close")??;
  let stdout = String::from_utf8_lossy(&stdout).into_owned();
  let stderr = String::from_utf8_lossy(&stderr);
  if echo {
    print!("{stdout}");
    eprint!("{stderr}");
  }
  match status {
    Some(status) if status.success() => Ok(stdout),
    Some(status) => bail!("{cmd:?} failed ({status}): {stderr}"),
    None => bail!("{cmd:?} exceeded {seconds}s deadline"),
  }
}

pub fn cargo(args: &[&str]) -> Result<()> {
  run(command("cargo").args(args), 1800).map(|_| ())
}

pub fn powershell(script: &str, args: &[String]) -> Result<()> {
  run(
    command("powershell.exe")
      .args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        script,
      ])
      .args(args),
    1800,
  )
  .map(|_| ())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn missing_command_reports_failure() {
    assert!(capture(&mut command("flinch-no-such-tool-verify"), 1).is_err());
  }
  #[test]
  #[cfg(windows)]
  fn both_pipes_are_drained_and_nonzero_exit_is_reported() -> Result<()> {
    let out = capture(
      command("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Out.Write(('a' * 100000)); [Console]::Error.Write(('b' * 100000))",
      ]),
      10,
    )?;
    assert_eq!(out.len(), 100000);
    assert!(
      capture(
        command("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "exit 7"]),
        10
      )
      .is_err()
    );
    Ok(())
  }
  #[test]
  #[cfg(windows)]
  fn overdue_child_is_terminated_within_a_bound() {
    let start = Instant::now();
    let result = capture(
      command("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep -Seconds 30",
      ]),
      1,
    );
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_secs(20));
  }
}
