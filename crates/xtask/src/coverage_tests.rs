use super::*;

#[test]
fn thresholds_use_exact_ratios_and_cannot_round_a_failure_into_a_pass() -> Result<()> {
  let policy = policy(80, 80, None);
  assert!(
    policy
      .enforce(Counts {
        covered: 799999,
        lines: 1000000
      })
      .is_err()
  );
  policy.enforce(Counts {
    covered: 800000,
    lines: 1000000,
  })?;
  policy.enforce(Counts {
    covered: 800001,
    lines: 1000000,
  })?;
  let policy = self::policy(80, 85, None);
  // A run may land up to 0.05 points under the baseline, exactly, and gains always pass.
  assert!(
    policy
      .enforce(Counts {
        covered: 849499,
        lines: 1000000
      })
      .is_err(),
    "more than the slack below the baseline is a regression"
  );
  policy.enforce(Counts {
    covered: 849500,
    lines: 1000000,
  })?;
  policy.enforce(Counts {
    covered: 86,
    lines: 100,
  })?;
  assert!(
    policy
      .enforce(Counts {
        covered: 84,
        lines: 100
      })
      .is_err(),
    "regressions cannot pass merely by remaining above 80%"
  );
  // Only a gain raises the baseline; a pass within the slack keeps it.
  assert!(policy.raised_by(Counts {
    covered: 851,
    lines: 1000
  }));
  assert!(!policy.raised_by(Counts {
    covered: 85,
    lines: 100
  }));
  assert!(!policy.raised_by(Counts {
    covered: 8497,
    lines: 10000
  }));
  // The slack never lowers the hard floor.
  let floor = self::policy(80, 80, None);
  assert!(
    floor
      .enforce(Counts {
        covered: 79999,
        lines: 100000
      })
      .is_err()
  );
  assert!(
    policy
      .enforce(Counts {
        covered: 0,
        lines: 0
      })
      .is_err()
  );
  assert!(
    policy
      .enforce(Counts {
        covered: 2,
        lines: 1
      })
      .is_err()
  );
  Ok(())
}

#[test]
fn policy_rejects_weakened_floor_erased_baseline_and_ambiguous_input() -> Result<()> {
  let valid = r#"{"minimum_line_percent":80,"baseline_covered_lines":17571,"baseline_instrumented_lines":29425}"#;
  let policy = Policy::parse(valid)?;
  assert!(
    policy.enforce(policy.baseline).is_err(),
    "the bootstrap measurement must still block release"
  );
  for input in [
    "{}",
    "null",
    r#"{"minimum_line_percent":79,"baseline_covered_lines":17571,"baseline_instrumented_lines":29425}"#,
    r#"{"minimum_line_percent":101,"baseline_covered_lines":17571,"baseline_instrumented_lines":29425}"#,
    r#"{"minimum_line_percent":80,"baseline_covered_lines":0,"baseline_instrumented_lines":1}"#,
    r#"{"minimum_line_percent":80,"baseline_covered_lines":10,"baseline_instrumented_lines":0}"#,
    r#"{"minimum_line_percent":80,"baseline_covered_lines":10,"baseline_instrumented_lines":1}"#,
    r#"{"minimum_line_percent":80,"baseline_covered_lines":17571,"baseline_instrumented_lines":29425,"skip":true}"#,
  ] {
    assert!(Policy::parse(input).is_err(), "accepted {input}");
  }
  Policy::parse(
    r#"{"minimum_line_percent":85,"baseline_covered_lines":90,"baseline_instrumented_lines":100}"#,
  )?;
  Ok(())
}

/// A policy with a baseline of `covered` per 100 lines.
fn policy(minimum: u64, covered: u64, reason: Option<&str>) -> Policy {
  Policy {
    minimum,
    baseline: Counts {
      covered,
      lines: 100,
    },
    rebaseline_reason: reason.map(str::to_owned),
  }
}

#[test]
fn committed_policy_cannot_be_reduced_even_when_current_coverage_matches_it() -> Result<()> {
  let previous = policy(80, 85, None);
  assert!(policy(80, 84, None).protect(&previous).is_err());
  assert!(policy(80, 90, None).protect(&policy(85, 85, None)).is_err());
  policy(85, 90, None).protect(&previous)?;
  Ok(())
}

/// The owner can reset the baseline only by stating a new reason; the floor never drops, and an
/// already committed reason cannot excuse a later drop.
#[test]
fn a_baseline_reset_needs_a_new_stated_reason_and_never_lowers_the_floor() -> Result<()> {
  let previous = policy(80, 85, None);
  let reset = policy(80, 81, Some("Re-measured with cargo-llvm-cov 0.9.1"));
  reset.protect(&previous)?;
  // The committed reset compared with itself, and a later raise without a reason, both pass.
  reset.protect(&reset)?;
  policy(80, 82, None).protect(&reset)?;
  // Reusing the committed reason for a further drop does not.
  assert!(
    policy(80, 80, Some("Re-measured with cargo-llvm-cov 0.9.1"))
      .protect(&reset)
      .is_err()
  );
  policy(80, 80, Some("Another stated reset")).protect(&reset)?;
  // No reason lowers the floor.
  assert!(
    policy(80, 90, Some("Lower the floor"))
      .protect(&policy(85, 85, None))
      .is_err()
  );
  // The reason is parsed strictly.
  let with = |reason: &str| {
    format!(
      r#"{{"minimum_line_percent":80,"baseline_covered_lines":17571,"baseline_instrumented_lines":29425,"rebaseline_reason":{reason}}}"#
    )
  };
  assert_eq!(
    Policy::parse(&with(r#"" Re-measured ""#))?
      .rebaseline_reason
      .as_deref(),
    Some("Re-measured")
  );
  for invalid in [
    r#""""#,
    r#""   ""#,
    "1",
    "null",
    &format!("\"{}\"", "x".repeat(301)),
  ] {
    assert!(Policy::parse(&with(invalid)).is_err(), "accepted {invalid}");
  }
  Ok(())
}

/// Regression: the ratchet rewrote the policy without the owner's reason, so ratcheting an
/// uncommitted reset made it fail the history guard as an unexplained drop.
#[test]
fn the_ratchet_keeps_the_owners_reset_reason() -> Result<()> {
  let committed = policy(80, 85, None);
  let reset = policy(80, 81, Some("Re-measured with cargo-llvm-cov 0.9.1"));
  let raised = Counts {
    covered: 82,
    lines: 100,
  };
  let ratcheted = Policy::parse(&reset.ratcheted(raised)?)?;
  assert_eq!(
    ratcheted.rebaseline_reason.as_deref(),
    Some("Re-measured with cargo-llvm-cov 0.9.1")
  );
  assert_eq!(
    (ratcheted.baseline.covered, ratcheted.baseline.lines),
    (82, 100)
  );
  assert_eq!(ratcheted.minimum, 80);
  ratcheted.protect(&committed)?;
  ratcheted.protect(&reset)?;
  // Without a reason, the ratchet writes none.
  let plain = Policy::parse(&committed.ratcheted(raised)?)?;
  assert_eq!(plain.rebaseline_reason, None);
  Ok(())
}

#[test]
fn ast_masks_tests_without_hiding_production_cfg_branches() -> Result<()> {
  let text = "fn production() {}\n#[cfg(test)] fn fixture() {}\nimpl X {\n #[cfg(test)] fn helper() {}\n fn real() {}\n}\n#[cfg(all(test, windows))] mod tests { mod external; }\n#[cfg(any(test, windows))] fn still_production() {}\nfn with_sink() {\n #[cfg(test)] if true { fixture(); }\n production();\n}\n";
  let mut mask = Mask {
    module_dir: PathBuf::from("src/sample"),
    file_dir: PathBuf::from("src"),
    ..Default::default()
  };
  mask.visit_file(&syn::parse_file(text)?);
  for line in [2, 4, 7, 10] {
    assert!(mask.contains(line), "test line {line} was included");
  }
  for line in [1, 5, 8, 11] {
    assert!(!mask.contains(line), "production line {line} was excluded");
  }
  assert!(
    mask
      .external
      .contains(&PathBuf::from("src/sample/tests/external.rs"))
  );
  let ast = syn::parse_file("#[cfg(test)] #[path=\"fixture.rs\"] mod renamed;")?;
  mask.visit_file(&ast);
  assert!(mask.external.contains(&PathBuf::from("src/fixture.rs")));
  Ok(())
}

#[test]
fn lcov_deduplicates_instantiations_and_excludes_fixture_functions() -> Result<()> {
  let output = workspace_root()?.join("target");
  fs::create_dir_all(&output)?;
  let temp = tempfile::Builder::new()
    .prefix("coverage-parser-")
    .tempdir_in(output)?;
  let path = temp.path().join("sample.rs");
  let text = "fn a() {}\nfn b() {}\n#[cfg(test)] fn test_fixture() {}\n";
  fs::write(&path, text)?;
  let path = path.canonicalize()?;
  let files = BTreeMap::from([(path.clone(), text.into())]);
  let sf = path.to_string_lossy();
  let data = format!(
    "SF:{sf}\nDA:1,0\nDA:2,0\nDA:3,999\nend_of_record\nSF:{sf}\nDA:1,2\nDA:2,0\nend_of_record\n"
  );
  let roots = [temp.path().canonicalize()?];
  let (counts, _) = measure(&data, &files, &roots)?;
  assert_eq!((counts.covered, counts.lines), (1, 2));
  for bad in [
    String::new(),
    format!("SF:{sf}\nDA:1,1\n"),
    format!("SF:{sf}\nDA:0,1\nend_of_record\n"),
    format!("SF:{sf}\nDA:4,1\nend_of_record\n"),
    format!("SF:{sf}\nDA:1,-1\nend_of_record\n"),
    format!("SF:{sf}\nSF:{sf}\nend_of_record\n"),
    "DA:1,1\nend_of_record\n".into(),
  ] {
    assert!(
      measure(&bad, &files, &roots).is_err(),
      "accepted malformed evidence: {bad}"
    );
  }
  Ok(())
}

#[test]
fn the_rustc_wrapper_instruments_argfile_crates_that_cargo_llvm_cov_missed() {
  // Regression: Cargo moved the `flinch` and `flinch-settings` command lines into an @argfile
  // and cargo-llvm-cov 0.9.1's wrapper passed them through uninstrumented.
  let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
  let argfile = "--crate-name\nflinch\n--edition=2024\nsrc\\lib.rs\n".to_string();
  assert!(needs_coverage_flags(
    &args(&["@C:\\tmp\\cargo-argfile.X"]),
    std::slice::from_ref(&argfile)
  ));
  assert!(needs_coverage_flags(
    &args(&["--crate-name", "flinch_blur"]),
    &[]
  ));
  // Already instrumented by cargo-llvm-cov, on the command line or inside the argfile.
  assert!(!needs_coverage_flags(
    &args(&[
      "--crate-name",
      "flinch_blur",
      "-Cinstrument-coverage",
      "--cfg=coverage"
    ]),
    &[]
  ));
  assert!(!needs_coverage_flags(
    &args(&["@C:\\tmp\\cargo-argfile.X"]),
    &[format!("{argfile}-C\ninstrument-coverage\n")]
  ));
  // Cargo's target-info probe and version queries are not crate builds.
  assert!(!needs_coverage_flags(
    &args(&["-", "--crate-name", "___", "--print=file-names"]),
    &[]
  ));
  assert!(!needs_coverage_flags(&args(&["-vV"]), &[]));
}

#[test]
fn a_production_root_without_coverage_data_fails_instead_of_leaving_the_denominator() -> Result<()>
{
  // Regression: under Rust 1.100, cargo-llvm-cov's wrapper skipped crates whose rustc command
  // line went into an @argfile. Their sources produced no LCOV records and would have
  // silently dropped out of the measured total.
  let output = workspace_root()?.join("target");
  fs::create_dir_all(&output)?;
  let temp = tempfile::Builder::new()
    .prefix("coverage-roots-")
    .tempdir_in(output)?;
  let measured = temp.path().join("measured");
  let missing = temp.path().join("missing");
  fs::create_dir_all(&measured)?;
  fs::create_dir_all(&missing)?;
  let text = "fn a() {}\n";
  fs::write(measured.join("lib.rs"), text)?;
  fs::write(missing.join("lib.rs"), text)?;
  let measured_file = measured.join("lib.rs").canonicalize()?;
  let missing_file = missing.join("lib.rs").canonicalize()?;
  let files = BTreeMap::from([
    (measured_file.clone(), text.to_string()),
    (missing_file, text.to_string()),
  ]);
  let data = format!(
    "SF:{}\nDA:1,1\nend_of_record\n",
    measured_file.to_string_lossy()
  );
  let roots = [measured.canonicalize()?, missing.canonicalize()?];
  let error = match measure(&data, &files, &roots) {
    Ok(_) => anyhow::bail!("accepted coverage that left out an uninstrumented root"),
    Err(error) => error.to_string(),
  };
  assert!(
    error.contains("No coverage data"),
    "unexpected error: {error}"
  );
  let (counts, _) = measure(&data, &files, &roots[..1])?;
  assert_eq!((counts.covered, counts.lines), (1, 1));
  Ok(())
}
