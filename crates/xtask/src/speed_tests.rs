#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::*;

/// Tests run from the crate folder; repository files are read from the root.
fn repo(path: &str) -> String {
  read(
    Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("../..")
      .join(path),
  )
  .expect("repository file")
}

fn baseline() -> Baseline {
  Baseline::parse(&repo(BASELINE)).expect("valid baseline")
}

#[test]
fn the_committed_baseline_parses_and_sits_under_its_ceilings() {
  let baseline = baseline();
  assert!(baseline.overlay_median_ms <= baseline.overlay_ceiling_ms);
  assert!(baseline.freeze_median_ms <= baseline.freeze_ceiling_ms);
  assert!(baseline.presses >= 20);
  assert!(baseline.tolerance_percent <= 20.0);
}

#[test]
fn malformed_or_meaningless_baselines_are_refused() {
  let mut value = baseline().to_json();
  for (key, bad) in [
    ("overlay_median_ms", json!(-1.0)),
    ("overlay_median_ms", json!(50.0)),
    ("freeze_ceiling_ms", json!("5")),
    ("tolerance_percent", json!(80.0)),
    ("presses", json!(2)),
    ("measured", Value::Null),
  ] {
    let original = value[key].clone();
    value[key] = bad;
    assert!(Baseline::parse(&value.to_string()).is_err(), "{key}");
    value[key] = original;
  }
  assert!(Baseline::parse("not json").is_err());
}

#[test]
fn limits_use_the_larger_of_percent_and_slack_and_never_pass_the_ceiling() {
  // 20% of 28.3 is 5.66 ms, more than 3 ms of slack.
  assert!((Baseline::limit(28.3, 20.0, 3.0, 40.0) - 33.96).abs() < 1e-9);
  // 20% of 1.56 is 0.31 ms, less than 1 ms of slack.
  assert!((Baseline::limit(1.56, 20.0, 1.0, 5.0) - 2.56).abs() < 1e-9);
  assert_eq!(Baseline::limit(38.0, 20.0, 3.0, 40.0), 40.0);
}

#[test]
fn snapshots_read_only_their_context_in_log_order() {
  let log = [
    r#"2026-11-18T10:00:00Z  INFO src\commands\system.rs: Speed snapshot screenshot.shortcut_to_freeze: 1.502ms operation="speed_snapshot" elapsed_ms="1.502""#,
    r#"2026-11-18T10:00:00Z  INFO src\commands\system.rs: Speed snapshot screenshot.shortcut_to_overlay: 29.125ms operation="speed_snapshot""#,
    "unrelated line",
    r#"2026-11-18T10:00:01Z  INFO src\commands\system.rs: Speed snapshot screenshot.shortcut_to_overlay: 27.000ms operation="speed_snapshot""#,
  ]
  .join("\n");
  assert_eq!(snapshots(&log, OVERLAY).expect("overlay"), [29.125, 27.0]);
  assert_eq!(snapshots(&log, FREEZE).expect("freeze"), [1.502]);
  assert!(snapshots("", OVERLAY).expect("empty").is_empty());
  assert!(
    snapshots(
      "Speed snapshot screenshot.shortcut_to_overlay: fast",
      OVERLAY
    )
    .is_err()
  );
  assert!(
    snapshots(
      "Speed snapshot screenshot.shortcut_to_overlay: -1ms",
      OVERLAY
    )
    .is_err()
  );
}

#[test]
fn medians_handle_odd_even_and_empty_inputs() {
  assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
  assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
  assert_eq!(median(&[]), None);
}

fn presses(first: f64, steady: f64, count: usize) -> Vec<f64> {
  std::iter::once(first)
    .chain(std::iter::repeat_n(steady, count))
    .collect()
}

#[test]
fn a_run_at_baseline_passes_and_each_regression_fails_on_its_own() {
  let baseline = baseline();
  let n = baseline.presses;
  let fine = evaluate(&baseline, &presses(33.0, 28.0, n), &presses(2.0, 1.5, n)).expect("run");
  assert!(fine.failures.is_empty(), "{:?}", fine.failures);
  assert_eq!(
    (fine.first_overlay_ms, fine.overlay_median_ms),
    (33.0, 28.0)
  );
  let slow_overlay =
    evaluate(&baseline, &presses(33.0, 36.0, n), &presses(2.0, 1.5, n)).expect("run");
  assert_eq!(slow_overlay.failures.len(), 1);
  assert!(slow_overlay.failures[0].contains("overlay median"));
  let slow_freeze =
    evaluate(&baseline, &presses(33.0, 28.0, n), &presses(2.0, 3.0, n)).expect("run");
  assert_eq!(slow_freeze.failures.len(), 1);
  assert!(slow_freeze.failures[0].contains("freeze median"));
  let slow_start =
    evaluate(&baseline, &presses(75.0, 28.0, n), &presses(2.0, 1.5, n)).expect("run");
  assert_eq!(slow_start.failures.len(), 1);
  assert!(slow_start.failures[0].contains("First overlay"));
  // One slow outlier does not move a median of twenty.
  let mut outlier = presses(33.0, 28.0, n);
  outlier[5] = 90.0;
  assert!(
    evaluate(&baseline, &outlier, &presses(2.0, 1.5, n))
      .expect("run")
      .failures
      .is_empty()
  );
}

#[test]
fn a_press_that_logged_no_stage_fails_instead_of_shrinking_the_sample() {
  let baseline = baseline();
  let n = baseline.presses;
  let error = evaluate(
    &baseline,
    &presses(33.0, 28.0, n - 1),
    &presses(2.0, 1.5, n),
  )
  .expect_err("missing overlay");
  assert!(error.to_string().contains("presses"));
  assert!(
    evaluate(
      &baseline,
      &presses(33.0, 28.0, n),
      &presses(2.0, 1.5, n + 1)
    )
    .is_err()
  );
}

#[test]
fn the_ratchet_only_lowers_medians_and_only_after_a_pass() {
  // Fixed medians, independent of whatever the committed baseline holds.
  let mut baseline = baseline();
  baseline.overlay_median_ms = 28.3;
  baseline.freeze_median_ms = 1.5;
  baseline.picker_overlay_median_ms = 35.0;
  baseline.picker_freeze_median_ms = 2.0;
  let n = baseline.presses;
  let faster = evaluate(&baseline, &presses(30.0, 25.004, n), &presses(2.0, 1.7, n)).expect("run");
  let picker = evaluate_tool(
    &baseline,
    PICKER,
    &presses(40.0, 31.126, n),
    &presses(3.0, 1.6, n),
  )
  .expect("picker run");
  let next = ratchet(&baseline, &faster, &picker, "test run".into()).expect("ratchet");
  assert_eq!(next.picker_overlay_median_ms, 31.13);
  assert_eq!(next.picker_freeze_median_ms, 1.6);
  assert_eq!(next.overlay_median_ms, 25.0);
  assert_eq!(
    next.freeze_median_ms, baseline.freeze_median_ms,
    "1.7 is slower"
  );
  assert_eq!(next.measured, "test run");
  next.protect(&baseline).expect("only lower");
  let failing = evaluate(&baseline, &presses(30.0, 39.0, n), &presses(2.0, 1.5, n)).expect("run");
  assert!(ratchet(&baseline, &failing, &picker, "x".into()).is_err());
  assert!(ratchet(&baseline, &picker, &failing, "x".into()).is_err());
}

#[test]
fn a_committed_baseline_cannot_loosen_without_a_recorded_reason() {
  let previous = baseline();
  let mut looser = previous.clone();
  looser.overlay_median_ms += 1.0;
  assert!(looser.protect(&previous).is_err());
  let mut wider = previous.clone();
  wider.tolerance_percent += 5.0;
  assert!(wider.protect(&previous).is_err());
  let mut fewer = previous.clone();
  fewer.presses -= 1;
  assert!(fewer.protect(&previous).is_err());
  looser.rebaseline_reason = Some("New reference machine".into());
  looser.protect(&previous).expect("reset with a reason");
  // A reason already in the previous commit does not excuse a further loosening.
  let mut again = looser.clone();
  again.overlay_median_ms += 1.0;
  assert!(again.protect(&looser).is_err());
}

#[test]
fn the_probe_config_assigns_only_its_shortcut_and_turns_off_outside_effects() {
  let defaults = repo("resources/defaultConfig.json");
  let config = probe_config(&defaults).expect("probe config");
  let object = config.as_object().expect("object");
  for (key, value) in object {
    if key.ends_with("_shortcut") {
      let expected = TOOLS
        .iter()
        .find(|tool| tool.config_key == key)
        .map_or("", |tool| tool.shortcut);
      assert_eq!(value, expected, "{key}");
    }
  }
  assert_eq!(config["check_for_updates"], false);
  assert_eq!(config["notifications_enabled"], false);
  assert_eq!(config["auto_start"], false);
  let defaults: Value = serde_json::from_str(&defaults).expect("defaults");
  assert_eq!(
    object.len(),
    defaults.as_object().expect("object").len(),
    "no keys added or lost"
  );
  assert!(probe_config("{}").is_err(), "a renamed setting is caught");
}

#[test]
fn readiness_and_conflicts_are_read_from_the_probe_log() {
  let line = |tool: Tool| {
    format!(
      r#"INFO src\shortcuts\handlers\screenshot.rs: Registered selection shortcut operation="register_selection_shortcut" shortcut_type="{}" shortcut={}"#,
      tool.shortcut_type, tool.shortcut
    )
  };
  let both = format!("{}\n{}", line(AREA), line(PICKER));
  assert!(registered(&both));
  assert!(
    !registered(&line(AREA)),
    "the Color Picker is not registered yet"
  );
  assert!(!registered(
    &both.replace(AREA.shortcut, "Ctrl+PrintScreen")
  ));
  assert!(!registered(
    &both.replace("screenshot_area", "screenshot_fullscreen")
  ));
  let line = line(AREA);
  assert!(conflicted(
    "WARN Some shortcuts remain conflicted after all retry attempts max_attempts=3 conflict_count=1"
  ));
  assert!(!conflicted(&line));
  assert!(!conflicted("INFO conflict_count=0"));
}

#[test]
fn the_probe_log_is_the_single_log_of_a_fresh_profile() -> Result<()> {
  let profile = tempfile::tempdir()?;
  assert_eq!(probe_log(profile.path())?, None);
  let logs = profile.path().join("logs");
  fs::create_dir_all(&logs)?;
  fs::write(logs.join("notes.txt"), "")?;
  assert_eq!(probe_log(profile.path())?, None);
  fs::write(logs.join("flinch_1.log"), "")?;
  assert_eq!(probe_log(profile.path())?, Some(logs.join("flinch_1.log")));
  fs::write(logs.join("flinch_2.log"), "")?;
  assert!(probe_log(profile.path()).is_err());
  Ok(())
}

#[test]
fn the_color_picker_is_held_to_its_own_medians_but_not_the_launch_ceiling() {
  let mut baseline = baseline();
  baseline.picker_overlay_median_ms = 31.0;
  baseline.picker_freeze_median_ms = 1.5;
  let n = baseline.presses;
  // Its first press is not the one after launch, so a slow one is only reported.
  let fine = evaluate_tool(
    &baseline,
    PICKER,
    &presses(75.0, 31.0, n),
    &presses(2.0, 1.5, n),
  )
  .expect("run");
  assert!(fine.failures.is_empty(), "{:?}", fine.failures);
  let slow = evaluate_tool(
    &baseline,
    PICKER,
    &presses(33.0, 38.0, n),
    &presses(2.0, 1.5, n),
  )
  .expect("run");
  assert_eq!(slow.failures.len(), 1);
  assert!(slow.failures[0].starts_with("Color Picker: "));
  // The area medians do not loosen the picker's, nor the other way round.
  baseline.overlay_median_ms = 20.0;
  let area = evaluate(&baseline, &presses(30.0, 28.0, n), &presses(2.0, 1.5, n)).expect("run");
  assert_eq!(area.failures.len(), 1);
  assert!(area.failures[0].starts_with("Shortcut to overlay"));
}

#[test]
fn each_tool_gets_its_own_presses_from_one_log() {
  let n = 5;
  let values: Vec<f64> = (0..(n + 1) * TOOLS.len())
    .map(|value| value as f64)
    .collect();
  assert_eq!(tool_presses(&values, 0, n).expect("area"), &values[..6]);
  assert_eq!(tool_presses(&values, 1, n).expect("picker"), &values[6..]);
  assert!(
    tool_presses(&values[1..], 0, n).is_err(),
    "a lost press fails"
  );
}

#[test]
fn the_color_picker_baseline_is_required_and_cannot_loosen_either() {
  let previous = baseline();
  let mut value = previous.to_json();
  value
    .as_object_mut()
    .expect("object")
    .remove("picker_overlay_median_ms");
  assert!(Baseline::parse(&value.to_string()).is_err());
  let mut looser = previous.clone();
  looser.picker_freeze_median_ms = previous.picker_freeze_median_ms + 0.5;
  assert!(looser.protect(&previous).is_err());
  let mut over = previous.to_json();
  over["picker_overlay_median_ms"] = json!(previous.overlay_ceiling_ms + 1.0);
  assert!(
    Baseline::parse(&over.to_string()).is_err(),
    "medians stay under the ceilings"
  );
}
