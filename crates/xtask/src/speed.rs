//! Overlay speed gate (`test:speed:desktop`) and its ratchet (`speed:ratchet`).
//!
//! A release build with the `speed-probe` feature starts in an isolated profile (its own
//! settings, logs and single-instance identity; see `src/probe_profile.rs`), so it runs beside a
//! running Flinch without touching it. The lab's `overlay-latency` presses the probe's area
//! capture shortcut, then its Color Picker shortcut, `PRESSES + 1` times each, and closes each
//! overlay with Escape, so nothing is saved or copied. The gate reads Flinch's own
//! `screenshot.shortcut_to_freeze` and `screenshot.shortcut_to_overlay` snapshots from the probe's
//! log, which time from the press reaching Flinch, and compares their medians with
//! `scripts/speed-baseline.json`. Each tool's first press is reported apart; the first press
//! after launch (area capture) is gated by its own ceiling.
//!
//! It opens real overlays over the desktop for about half a minute, so it is a desktop command:
//! never part of quiet checks.
use crate::{process, read};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
  fs,
  path::{Path, PathBuf},
  process::{Child, Stdio},
  time::{Duration, Instant},
};

pub const BASELINE: &str = "scripts/speed-baseline.json";
/// A shortcut the probe presses: its settings key, the shortcut type its registration logs, and
/// keys chosen to collide with nothing a user is likely to assign.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tool {
  pub name: &'static str,
  pub config_key: &'static str,
  pub shortcut_type: &'static str,
  pub shortcut: &'static str,
}
pub const AREA: Tool = Tool {
  name: "Area capture",
  config_key: "screenshot_area_shortcut",
  shortcut_type: "screenshot_area",
  shortcut: "Ctrl+Alt+Shift+F9",
};
pub const PICKER: Tool = Tool {
  name: "Color Picker",
  config_key: "color_picker_shortcut",
  shortcut_type: "pick_color",
  shortcut: "Ctrl+Alt+Shift+F10",
};
/// In the order the probe presses them; the first one's first press is the one after launch.
pub const TOOLS: [Tool; 2] = [AREA, PICKER];
const PROBE_TARGET: &str = "target/speed-probe";
const PROFILE_ENV: &str = "FLINCH_PROBE_PROFILE";
const OVERLAY: &str = "screenshot.shortcut_to_overlay";
const FREEZE: &str = "screenshot.shortcut_to_freeze";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// After registration, startup warming (capture, overlay resources) settles before the first press.
const SETTLE: Duration = Duration::from_secs(3);
/// Lab runs: one press per second plus the overlay hold, for every press.
const LAB_TIMEOUT_SECONDS: u64 = 180;

#[derive(Clone, Debug, PartialEq)]
pub struct Baseline {
  pub overlay_median_ms: f64,
  pub freeze_median_ms: f64,
  pub picker_overlay_median_ms: f64,
  pub picker_freeze_median_ms: f64,
  pub overlay_ceiling_ms: f64,
  pub freeze_ceiling_ms: f64,
  pub first_overlay_ceiling_ms: f64,
  pub tolerance_percent: f64,
  pub overlay_slack_ms: f64,
  pub freeze_slack_ms: f64,
  pub presses: usize,
  pub measured: String,
  pub rebaseline_reason: Option<String>,
}

impl Baseline {
  pub fn parse(text: &str) -> Result<Self> {
    let value: Value = serde_json::from_str(text).context("Parse speed baseline")?;
    let number = |key: &str| -> Result<f64> {
      let number = value[key]
        .as_f64()
        .with_context(|| format!("Speed baseline needs a number for {key}"))?;
      ensure!(
        number.is_finite() && number > 0.0,
        "Speed baseline {key} must be positive"
      );
      Ok(number)
    };
    let baseline = Self {
      overlay_median_ms: number("overlay_median_ms")?,
      freeze_median_ms: number("freeze_median_ms")?,
      picker_overlay_median_ms: number("picker_overlay_median_ms")?,
      picker_freeze_median_ms: number("picker_freeze_median_ms")?,
      overlay_ceiling_ms: number("overlay_ceiling_ms")?,
      freeze_ceiling_ms: number("freeze_ceiling_ms")?,
      first_overlay_ceiling_ms: number("first_overlay_ceiling_ms")?,
      tolerance_percent: number("tolerance_percent")?,
      overlay_slack_ms: number("overlay_slack_ms")?,
      freeze_slack_ms: number("freeze_slack_ms")?,
      presses: value["presses"]
        .as_u64()
        .filter(|presses| (5..=100).contains(presses))
        .context("Speed baseline presses must be 5 to 100")? as usize,
      measured: value["measured"]
        .as_str()
        .context("Speed baseline needs a measured note")?
        .to_owned(),
      rebaseline_reason: value["rebaseline_reason"].as_str().map(str::to_owned),
    };
    ensure!(
      baseline
        .overlay_median_ms
        .max(baseline.picker_overlay_median_ms)
        <= baseline.overlay_ceiling_ms
        && baseline
          .freeze_median_ms
          .max(baseline.picker_freeze_median_ms)
          <= baseline.freeze_ceiling_ms,
      "Speed baseline medians must lie under their ceilings"
    );
    ensure!(
      baseline.tolerance_percent <= 50.0,
      "Speed tolerance above 50% would not guard anything"
    );
    Ok(baseline)
  }

  pub fn to_json(&self) -> Value {
    json!({
      "overlay_median_ms": self.overlay_median_ms,
      "freeze_median_ms": self.freeze_median_ms,
      "picker_overlay_median_ms": self.picker_overlay_median_ms,
      "picker_freeze_median_ms": self.picker_freeze_median_ms,
      "overlay_ceiling_ms": self.overlay_ceiling_ms,
      "freeze_ceiling_ms": self.freeze_ceiling_ms,
      "first_overlay_ceiling_ms": self.first_overlay_ceiling_ms,
      "tolerance_percent": self.tolerance_percent,
      "overlay_slack_ms": self.overlay_slack_ms,
      "freeze_slack_ms": self.freeze_slack_ms,
      "presses": self.presses,
      "measured": self.measured,
      "rebaseline_reason": self.rebaseline_reason,
    })
  }

  /// The slowest median that still passes: the baseline plus its tolerance (or slack, whichever
  /// is larger, so a 1.5 ms stage is not failed by scheduler noise), never above the ceiling.
  pub fn limit(median: f64, tolerance_percent: f64, slack: f64, ceiling: f64) -> f64 {
    (median * (1.0 + tolerance_percent / 100.0))
      .max(median + slack)
      .min(ceiling)
  }

  /// Like the coverage floor, a committed baseline only gets faster or stricter, unless its owner
  /// records why it was reset.
  pub fn protect(&self, previous: &Self) -> Result<()> {
    if self.rebaseline_reason.is_some() && self.rebaseline_reason != previous.rebaseline_reason {
      return Ok(());
    }
    for (name, now, before) in [
      (
        "overlay_median_ms",
        self.overlay_median_ms,
        previous.overlay_median_ms,
      ),
      (
        "freeze_median_ms",
        self.freeze_median_ms,
        previous.freeze_median_ms,
      ),
      (
        "picker_overlay_median_ms",
        self.picker_overlay_median_ms,
        previous.picker_overlay_median_ms,
      ),
      (
        "picker_freeze_median_ms",
        self.picker_freeze_median_ms,
        previous.picker_freeze_median_ms,
      ),
      (
        "overlay_ceiling_ms",
        self.overlay_ceiling_ms,
        previous.overlay_ceiling_ms,
      ),
      (
        "freeze_ceiling_ms",
        self.freeze_ceiling_ms,
        previous.freeze_ceiling_ms,
      ),
      (
        "first_overlay_ceiling_ms",
        self.first_overlay_ceiling_ms,
        previous.first_overlay_ceiling_ms,
      ),
      (
        "tolerance_percent",
        self.tolerance_percent,
        previous.tolerance_percent,
      ),
      (
        "overlay_slack_ms",
        self.overlay_slack_ms,
        previous.overlay_slack_ms,
      ),
      (
        "freeze_slack_ms",
        self.freeze_slack_ms,
        previous.freeze_slack_ms,
      ),
    ] {
      ensure!(
        now <= before,
        "Speed baseline {name} rose from {before} to {now}; record rebaseline_reason to reset it"
      );
    }
    ensure!(
      self.presses >= previous.presses,
      "Speed baseline presses fell from {} to {}",
      previous.presses,
      self.presses
    );
    Ok(())
  }
}

/// Every `Speed snapshot CONTEXT: N.NNNms` value in a Flinch log, in order.
pub fn snapshots(log: &str, context: &str) -> Result<Vec<f64>> {
  let marker = format!("Speed snapshot {context}: ");
  let mut values = Vec::new();
  for line in log.lines() {
    let Some((_, rest)) = line.split_once(&marker) else {
      continue;
    };
    let number = rest
      .split_once("ms")
      .map(|(number, _)| number)
      .with_context(|| format!("Malformed speed snapshot: {line}"))?;
    let value: f64 = number
      .trim()
      .parse()
      .with_context(|| format!("Malformed speed snapshot: {line}"))?;
    ensure!(
      value.is_finite() && value >= 0.0,
      "Invalid speed snapshot: {line}"
    );
    values.push(value);
  }
  Ok(values)
}

pub fn median(values: &[f64]) -> Option<f64> {
  let mut sorted = values.to_vec();
  sorted.sort_by(f64::total_cmp);
  let middle = sorted.len() / 2;
  match sorted.len() {
    0 => None,
    length if length % 2 == 1 => Some(sorted[middle]),
    _ => Some((sorted[middle - 1] + sorted[middle]) / 2.0),
  }
}

#[derive(Debug, PartialEq)]
pub struct Verdict {
  pub first_overlay_ms: f64,
  pub first_freeze_ms: f64,
  pub overlay_median_ms: f64,
  pub freeze_median_ms: f64,
  pub overlay_limit_ms: f64,
  pub freeze_limit_ms: f64,
  pub failures: Vec<String>,
}

impl Baseline {
  /// This tool's recorded overlay and freeze medians.
  pub fn medians(&self, tool: Tool) -> (f64, f64) {
    if tool == PICKER {
      (self.picker_overlay_median_ms, self.picker_freeze_median_ms)
    } else {
      (self.overlay_median_ms, self.freeze_median_ms)
    }
  }
}

/// Area capture's presses, the first of them right after launch.
#[cfg(test)]
pub fn evaluate(baseline: &Baseline, overlay: &[f64], freeze: &[f64]) -> Result<Verdict> {
  evaluate_tool(baseline, AREA, overlay, freeze)
}

/// Splits a tool's first press from its steady presses and checks them against `baseline`.
/// Only the first tool's first press follows launch, so only it is held to the launch ceiling.
pub fn evaluate_tool(
  baseline: &Baseline,
  tool: Tool,
  overlay: &[f64],
  freeze: &[f64],
) -> Result<Verdict> {
  let (base_overlay, base_freeze) = baseline.medians(tool);
  let prefix = if tool == AREA {
    String::new()
  } else {
    format!("{}: ", tool.name)
  };
  let expected = baseline.presses + 1;
  ensure!(
    overlay.len() == expected && freeze.len() == expected,
    "Expected {expected} presses to log both stages; the log has {} overlay and {} freeze snapshots",
    overlay.len(),
    freeze.len()
  );
  let overlay_median_ms = median(&overlay[1..]).context("No steady overlay presses")?;
  let freeze_median_ms = median(&freeze[1..]).context("No steady freeze presses")?;
  let overlay_limit_ms = Baseline::limit(
    base_overlay,
    baseline.tolerance_percent,
    baseline.overlay_slack_ms,
    baseline.overlay_ceiling_ms,
  );
  let freeze_limit_ms = Baseline::limit(
    base_freeze,
    baseline.tolerance_percent,
    baseline.freeze_slack_ms,
    baseline.freeze_ceiling_ms,
  );
  let mut failures = Vec::new();
  if overlay_median_ms > overlay_limit_ms {
    failures.push(format!(
      "{prefix}Shortcut to overlay median {overlay_median_ms:.2} ms exceeds {overlay_limit_ms:.2} ms (baseline {base_overlay:.2} ms)"
    ));
  }
  if freeze_median_ms > freeze_limit_ms {
    failures.push(format!(
      "{prefix}Shortcut to freeze median {freeze_median_ms:.2} ms exceeds {freeze_limit_ms:.2} ms (baseline {base_freeze:.2} ms)"
    ));
  }
  if tool == TOOLS[0] && overlay[0] > baseline.first_overlay_ceiling_ms {
    failures.push(format!(
      "First overlay after launch took {:.2} ms, over its {:.2} ms ceiling",
      overlay[0], baseline.first_overlay_ceiling_ms
    ));
  }
  Ok(Verdict {
    first_overlay_ms: overlay[0],
    first_freeze_ms: freeze[0],
    overlay_median_ms,
    freeze_median_ms,
    overlay_limit_ms,
    freeze_limit_ms,
    failures,
  })
}

/// A passing run lowers the medians it beat; nothing ever rises.
pub fn ratchet(
  baseline: &Baseline,
  area: &Verdict,
  picker: &Verdict,
  measured: String,
) -> Result<Baseline> {
  ensure!(
    area.failures.is_empty() && picker.failures.is_empty(),
    "Only a passing run can move the speed baseline"
  );
  let lower = |measured: f64, recorded: f64| round(measured.min(recorded));
  let mut next = baseline.clone();
  next.overlay_median_ms = lower(area.overlay_median_ms, baseline.overlay_median_ms);
  next.freeze_median_ms = lower(area.freeze_median_ms, baseline.freeze_median_ms);
  next.picker_overlay_median_ms =
    lower(picker.overlay_median_ms, baseline.picker_overlay_median_ms);
  next.picker_freeze_median_ms = lower(picker.freeze_median_ms, baseline.picker_freeze_median_ms);
  next.measured = measured;
  next.rebaseline_reason = None;
  Ok(next)
}

fn round(value: f64) -> f64 {
  (value * 100.0).round() / 100.0
}

/// The shipped defaults with only the probe's shortcuts assigned and nothing that reaches the
/// network or another window: no update check, notifications or autostart.
pub fn probe_config(defaults: &str) -> Result<Value> {
  let mut config: Value = serde_json::from_str(defaults).context("Parse default configuration")?;
  let object = config
    .as_object_mut()
    .context("Default configuration is not an object")?;
  for (key, value) in object.iter_mut() {
    if key.ends_with("_shortcut") {
      *value = json!("");
    }
  }
  for (key, value) in [
    (AREA.config_key, json!(AREA.shortcut)),
    (PICKER.config_key, json!(PICKER.shortcut)),
    ("check_for_updates", json!(false)),
    ("notifications_enabled", json!(false)),
    ("auto_start", json!(false)),
    ("language", json!("en")),
  ] {
    ensure!(
      object.contains_key(key),
      "Default configuration lacks {key}"
    );
    object.insert(key.to_owned(), value);
  }
  Ok(config)
}

/// Whether the probe's log shows every probe shortcut registered.
pub fn registered(log: &str) -> bool {
  TOOLS.iter().all(|tool| {
    log.lines().any(|line| {
      line.contains("Registered selection shortcut")
        && line.contains(&format!("shortcut_type=\"{}\"", tool.shortcut_type))
        && line.contains(&format!("shortcut={}", tool.shortcut))
    })
  })
}

/// One tool's presses from a log that holds every tool's, in press order.
pub fn tool_presses(values: &[f64], index: usize, presses: usize) -> Result<&[f64]> {
  let each = presses + 1;
  ensure!(
    values.len() == each * TOOLS.len(),
    "Expected {each} presses for each of {} tools to log both stages; the log has {}",
    TOOLS.len(),
    values.len()
  );
  Ok(&values[index * each..(index + 1) * each])
}

/// A registration conflict means another program owns the probe's shortcut.
pub fn conflicted(log: &str) -> bool {
  log
    .lines()
    .any(|line| line.contains("conflict_count") && !line.contains("conflict_count=0"))
}

/// The single log file a fresh probe profile holds.
fn probe_log(profile: &Path) -> Result<Option<PathBuf>> {
  let folder = profile.join("logs");
  if !folder.is_dir() {
    return Ok(None);
  }
  let mut logs = Vec::new();
  for entry in fs::read_dir(&folder).context("List probe logs")? {
    let path = entry?.path();
    if path.extension().is_some_and(|extension| extension == "log") {
      logs.push(path);
    }
  }
  ensure!(
    logs.len() <= 1,
    "A fresh probe profile holds one log, not {}",
    logs.len()
  );
  Ok(logs.pop())
}

/// Ends the probe this run started, however the run ends. It owns exactly this child.
struct Probe(Option<Child>);
impl Probe {
  fn alive(&mut self) -> Result<bool> {
    match self.0.as_mut() {
      Some(child) => Ok(child.try_wait()?.is_none()),
      None => Ok(false),
    }
  }
  fn stop(&mut self) -> Result<()> {
    let Some(mut child) = self.0.take() else {
      return Ok(());
    };
    if child.try_wait()?.is_none() {
      child.kill().context("End the speed probe")?;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait()?.is_none() {
      ensure!(Instant::now() < deadline, "The speed probe did not exit");
      std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
  }
}
impl Drop for Probe {
  fn drop(&mut self) {
    if let Err(error) = self.stop() {
      eprintln!("Speed probe cleanup: {error:#}");
    }
  }
}

fn nonce() -> Result<u128> {
  Ok(
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)?
      .as_millis(),
  )
}

pub fn run(ratchet_baseline: bool) -> Result<()> {
  let baseline_text = read(BASELINE)?;
  let baseline = Baseline::parse(&baseline_text)?;
  protect_history(&baseline)?;
  process::cargo(&[
    "build",
    "--release",
    "--bin",
    "flinch",
    "--features",
    "speed-probe",
    "--target-dir",
    PROBE_TARGET,
  ])?;
  process::cargo(&["build", "--release", "-p", "flinch-video-lab"])?;
  let executable = std::env::current_dir()?
    .join(PROBE_TARGET)
    .join("release/flinch.exe");
  let lab = std::env::current_dir()?.join("target/release/flinch-video-lab.exe");
  let profile = std::env::current_dir()?
    .join(PROBE_TARGET)
    .join(format!("profile-{}", nonce()?));
  fs::create_dir_all(&profile)?;
  let config = probe_config(&read("resources/defaultConfig.json")?)?;
  fs::write(
    profile.join("config.json"),
    serde_json::to_vec_pretty(&config)?,
  )?;
  println!("Speed probe profile: {}", profile.display());

  let mut probe = Probe(Some(
    process::command(&executable)
      .env(PROFILE_ENV, &profile)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .with_context(|| format!("Start {}", executable.display()))?,
  ));
  let started = Instant::now();
  loop {
    ensure!(probe.alive()?, "The speed probe exited during startup");
    if let Some(log) = probe_log(&profile)? {
      let text = read(&log)?;
      ensure!(
        !conflicted(&text),
        "Another program holds a probe shortcut ({} or {}); the probe cannot register it",
        AREA.shortcut,
        PICKER.shortcut
      );
      if registered(&text) {
        break;
      }
    }
    ensure!(
      started.elapsed() < READY_TIMEOUT,
      "The speed probe did not register {} and {} within {} s",
      AREA.shortcut,
      PICKER.shortcut,
      READY_TIMEOUT.as_secs()
    );
    std::thread::sleep(Duration::from_millis(100));
  }
  std::thread::sleep(SETTLE);
  let mut labs = Vec::new();
  for tool in TOOLS {
    let lab_report = profile.join(format!("overlay-latency-{}.json", tool.shortcut_type));
    process::run(
      process::command(&lab).args([
        "overlay-latency",
        "--tool",
        tool.name,
        "--process",
        "flinch.exe",
        "--keys",
        tool.shortcut,
        "--runs",
        &baseline.presses.to_string(),
        "--gap-ms",
        "600",
        "--output",
        &lab_report.to_string_lossy(),
      ]),
      LAB_TIMEOUT_SECONDS,
    )?;
    ensure!(
      probe.alive()?,
      "The speed probe exited during the {} presses",
      tool.name
    );
    let report: Value = crate::json(&lab_report)?;
    ensure!(
      report["timeouts"].as_u64() == Some(0) && report["stopped_early"].is_null(),
      "The {} overlay did not open or close for every press: {}",
      tool.name,
      lab_report.display()
    );
    labs.push(report);
  }
  probe.stop()?;

  let log = probe_log(&profile)?.context("The speed probe wrote no log")?;
  let text = read(&log)?;
  let overlay = snapshots(&text, OVERLAY)?;
  let freeze = snapshots(&text, FREEZE)?;
  let mut verdicts = Vec::new();
  let mut tools = Vec::new();
  for (index, tool) in TOOLS.into_iter().enumerate() {
    let tool_overlay = tool_presses(&overlay, index, baseline.presses)?;
    let tool_freeze = tool_presses(&freeze, index, baseline.presses)?;
    let verdict = evaluate_tool(&baseline, tool, tool_overlay, tool_freeze)?;
    println!(
      "{}: freeze median {:.2} ms (limit {:.2}), first {:.2} ms",
      tool.name, verdict.freeze_median_ms, verdict.freeze_limit_ms, verdict.first_freeze_ms
    );
    println!(
      "{}: overlay median {:.2} ms (limit {:.2}), first {:.2} ms",
      tool.name, verdict.overlay_median_ms, verdict.overlay_limit_ms, verdict.first_overlay_ms
    );
    tools.push(json!({
      "tool": tool.name,
      "first_overlay_ms": verdict.first_overlay_ms,
      "first_freeze_ms": verdict.first_freeze_ms,
      "overlay_median_ms": verdict.overlay_median_ms,
      "overlay_limit_ms": verdict.overlay_limit_ms,
      "freeze_median_ms": verdict.freeze_median_ms,
      "freeze_limit_ms": verdict.freeze_limit_ms,
      "overlay_ms": tool_overlay,
      "freeze_ms": tool_freeze,
      "window_visible_median_ms": labs[index]["median_ms"],
      "failures": verdict.failures,
    }));
    verdicts.push(verdict);
  }
  let screen = format!("{}x{}", labs[0]["screen"][0], labs[0]["screen"][1]);
  println!(
    "First overlay after launch: {:.2} ms (ceiling {:.2})",
    verdicts[0].first_overlay_ms, baseline.first_overlay_ceiling_ms
  );
  let failures: Vec<String> = verdicts
    .iter()
    .flat_map(|verdict| verdict.failures.iter().cloned())
    .collect();
  let report = json!({
    "baseline": baseline.to_json(),
    "screen": screen,
    "tools": tools,
    "failures": failures,
  });
  fs::write(
    profile.join("speed-report.json"),
    serde_json::to_vec_pretty(&report)?,
  )?;
  if !failures.is_empty() {
    bail!(
      "Overlay speed regressed:\n{}\nReport: {}",
      failures.join("\n"),
      profile.join("speed-report.json").display()
    );
  }
  if ratchet_baseline {
    let next = ratchet(
      &baseline,
      &verdicts[0],
      &verdicts[1],
      format!(
        "speed:ratchet, release probe build, {screen}, {} presses, unix time {}",
        baseline.presses,
        nonce()? / 1000
      ),
    )?;
    next.protect(&baseline)?;
    fs::write(BASELINE, format!("{:#}\n", next.to_json()))?;
    println!(
      "Speed baseline: area overlay {:.2} -> {:.2} ms, freeze {:.2} -> {:.2} ms; Color Picker overlay {:.2} -> {:.2} ms, freeze {:.2} -> {:.2} ms",
      baseline.overlay_median_ms,
      next.overlay_median_ms,
      baseline.freeze_median_ms,
      next.freeze_median_ms,
      baseline.picker_overlay_median_ms,
      next.picker_overlay_median_ms,
      baseline.picker_freeze_median_ms,
      next.picker_freeze_median_ms
    );
  }
  println!("Overlay speed passed");
  Ok(())
}

/// The committed baseline must not have been loosened since the previous commit.
fn protect_history(baseline: &Baseline) -> Result<()> {
  let entry = process::capture(
    process::command("git").args(["ls-tree", "--name-only", "HEAD", "--", BASELINE]),
    10,
  )?;
  if entry.trim().is_empty() {
    return Ok(());
  }
  let previous = process::capture(
    process::command("git").args(["show", &format!("HEAD:{BASELINE}")]),
    10,
  )?;
  baseline.protect(&Baseline::parse(&previous)?)
}

#[cfg(test)]
#[path = "speed_tests.rs"]
mod tests;
