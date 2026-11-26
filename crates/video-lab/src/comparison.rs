//! Offline frame-time imports. No collector is installed or launched.
use flinch_video_engine::{EngineError, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, io::Read, path::Path};
#[derive(Serialize, Deserialize)]
pub struct Conditions {
  pub scene: String,
  pub resolution: String,
  pub game_settings: String,
  pub video_quality: String,
  pub fps_limit: String,
  pub gpu_load: String,
  pub warmed_up: bool,
  pub frame_time_column: String,
}
#[derive(Serialize)]
pub struct Pair {
  pub baseline_median_ms: f64,
  pub recorded_median_ms: f64,
  pub baseline_p99_ms: f64,
  pub recorded_p99_ms: f64,
  pub fps_loss_percent: f64,
  pub p99_regression_percent: f64,
}
#[derive(Serialize)]
pub struct Comparison {
  pub conditions: Conditions,
  pub pairs: Vec<Pair>,
  pub median_fps_loss_percent: f64,
  pub median_p99_regression_percent: f64,
  pub within_initial_impact_limits: bool,
  pub superiority_established: bool,
}
fn fields(line: &str) -> Result<Vec<String>> {
  let mut fields = Vec::new();
  let mut text = String::new();
  let mut quoted = false;
  let mut chars = line.chars().peekable();
  while let Some(c) = chars.next() {
    match c {
      '"' => {
        if quoted && chars.peek() == Some(&'"') {
          text.push('"');
          chars.next();
        } else {
          quoted = !quoted;
        }
      },
      ',' if !quoted => fields.push(std::mem::take(&mut text)),
      _ => text.push(c),
    }
  }
  if quoted {
    return Err(EngineError::protocol("Unclosed CSV quote"));
  }
  fields.push(text);
  Ok(fields)
}
fn quantile(values: &mut [f64], percent: usize) -> f64 {
  values.sort_by(f64::total_cmp);
  values[(values.len() * percent).div_ceil(100) - 1]
}
pub fn parse_csv(text: &str, column: &str) -> Result<(f64, f64)> {
  let mut lines = text.trim_start_matches('\u{feff}').lines();
  let header = fields(
    lines
      .next()
      .ok_or_else(|| EngineError::protocol("Empty frame-time CSV"))?,
  )?;
  let positions: Vec<_> = header
    .iter()
    .enumerate()
    .filter(|(_, v)| v.trim() == column)
    .map(|(i, _)| i)
    .collect();
  if positions.len() != 1 {
    return Err(EngineError::protocol(
      "Requested frame-time column must occur exactly once",
    ));
  }
  let index = positions[0];
  let mut values = Vec::<f64>::new();
  for line in lines {
    if line.trim().is_empty() {
      continue;
    }
    let row = fields(line)?;
    if row.len() != header.len() {
      return Err(EngineError::protocol("CSV row/header field count differs"));
    }
    values.push(
      row[index]
        .trim()
        .parse()
        .map_err(|_| EngineError::protocol("Invalid numeric frame time"))?,
    );
    if values.len() > 2_000_000 {
      return Err(EngineError::protocol(
        "Frame-time CSV exceeds two million rows",
      ));
    }
  }
  if values.len() < 120 || values.iter().any(|v| !v.is_finite() || *v <= 0.0) {
    return Err(EngineError::protocol(
      "At least 120 positive finite frame times are required",
    ));
  }
  let median = quantile(&mut values, 50);
  let tail = quantile(&mut values, 99);
  Ok((median, tail))
}
fn read(path: &Path) -> Result<String> {
  let mut file = std::fs::File::open(path).map_err(|e| EngineError::io("comparison file", e))?;
  let mut text = String::new();
  file
    .by_ref()
    .take(16 * 1024 * 1024 + 1)
    .read_to_string(&mut text)
    .map_err(|e| EngineError::io("comparison CSV", e))?;
  if text.len() > 16 * 1024 * 1024 {
    return Err(EngineError::protocol("Comparison input exceeds 16 MiB"));
  }
  Ok(text)
}
pub fn compare(args: &[OsString]) -> Result<Comparison> {
  if args.len() < 7 || args.len().is_multiple_of(2) {
    return Err(EngineError::protocol(
      "compare --frames requires CONDITIONS.json and at least three BASELINE.csv RECORDED.csv pairs",
    ));
  }
  let conditions: Conditions = serde_json::from_str(&read(Path::new(&args[0]))?)
    .map_err(|e| EngineError::protocol(e.to_string()))?;
  if !conditions.warmed_up
    || [
      &conditions.scene,
      &conditions.resolution,
      &conditions.game_settings,
      &conditions.video_quality,
      &conditions.fps_limit,
      &conditions.gpu_load,
      &conditions.frame_time_column,
    ]
    .iter()
    .any(|s| s.trim().is_empty())
  {
    return Err(EngineError::protocol(
      "Benchmark conditions must be explicit and runs warmed up",
    ));
  }
  let mut pairs = Vec::new();
  for pair in args[1..].as_chunks::<2>().0 {
    let (bm, bp) = parse_csv(&read(Path::new(&pair[0]))?, &conditions.frame_time_column)?;
    let (rm, rp) = parse_csv(&read(Path::new(&pair[1]))?, &conditions.frame_time_column)?;
    pairs.push(Pair {
      baseline_median_ms: bm,
      recorded_median_ms: rm,
      baseline_p99_ms: bp,
      recorded_p99_ms: rp,
      fps_loss_percent: (1.0 - bm / rm) * 100.0,
      p99_regression_percent: (rp / bp - 1.0) * 100.0,
    });
  }
  let fps = quantile(
    &mut pairs.iter().map(|p| p.fps_loss_percent).collect::<Vec<_>>(),
    50,
  );
  let tail = quantile(
    &mut pairs
      .iter()
      .map(|p| p.p99_regression_percent)
      .collect::<Vec<_>>(),
    50,
  );
  Ok(Comparison {
    conditions,
    pairs,
    median_fps_loss_percent: fps,
    median_p99_regression_percent: tail,
    within_initial_impact_limits: fps <= 3.0 && tail <= 5.0,
    superiority_established: false,
  })
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn paired_imports_require_explicit_conditions_and_detect_excessive_game_impact() {
    let root = tempfile::tempdir().expect("temp");
    let conditions = root.path().join("conditions.json");
    let baseline = root.path().join("baseline.csv");
    let recording = root.path().join("recording.csv");
    let mut specification = Conditions {
      scene: "replay".into(),
      resolution: "2560x1440".into(),
      game_settings: "high".into(),
      video_quality: "CQP 21/23".into(),
      fps_limit: "120".into(),
      gpu_load: "headroom".into(),
      warmed_up: true,
      frame_time_column: "MsBetweenPresents".into(),
    };
    std::fs::write(
      &conditions,
      serde_json::to_vec(&specification).expect("conditions"),
    )
    .expect("write conditions");
    std::fs::write(
      &baseline,
      format!("MsBetweenPresents\n{}", "10\n".repeat(200)),
    )
    .expect("baseline");
    let mut args = vec![conditions.clone().into_os_string()];
    for _ in 0..3 {
      args.extend([
        baseline.clone().into_os_string(),
        recording.clone().into_os_string(),
      ]);
    }
    for (value, accepted) in [(10.1, true), (10.6, false)] {
      std::fs::write(
        &recording,
        format!("MsBetweenPresents\n{}", format!("{value}\n").repeat(200)),
      )
      .expect("recorded frame times");
      let report = compare(&args).expect("paired comparison");
      assert_eq!(report.pairs.len(), 3);
      assert_eq!(report.within_initial_impact_limits, accepted);
      assert!(!report.superiority_established);
      assert!((report.median_fps_loss_percent - (1.0 - 10.0 / value) * 100.0).abs() < 1e-10);
      assert!((report.median_p99_regression_percent - (value / 10.0 - 1.0) * 100.0).abs() < 1e-10);
    }
    specification.warmed_up = false;
    std::fs::write(
      &conditions,
      serde_json::to_vec(&specification).expect("conditions"),
    )
    .expect("write conditions");
    assert!(compare(&args).is_err());
    assert!(compare(&args[..5]).is_err());
  }
  #[test]
  fn parses_presentmon_quoted_headers_and_rejects_invalid_measurements() {
    let text = format!(
      "Application,\"Frame Time\"\n{}",
      "\"game, x\",10\n".repeat(200)
    );
    assert_eq!(
      parse_csv(&text, "Frame Time").expect("valid CSV"),
      (10.0, 10.0)
    );
    assert!(parse_csv(&text, "missing").is_err());
    assert!(parse_csv(&text.replace(",10", ",NaN"), "Frame Time").is_err());
    assert!(fields("\"broken").is_err());
    assert!(parse_csv("A,A\n1,2", "A").is_err());
  }
}
