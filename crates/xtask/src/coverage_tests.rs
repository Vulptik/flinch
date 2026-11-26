use super::*;

#[test]
fn the_floor_uses_exact_ratios_and_cannot_round_a_failure_into_a_pass() -> Result<()> {
  let policy = Policy { minimum: 80 };
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
    covered: 86,
    lines: 100,
  })?;
  for invalid in [
    Counts {
      covered: 0,
      lines: 0,
    },
    Counts {
      covered: 2,
      lines: 1,
    },
  ] {
    assert!(policy.enforce(invalid).is_err());
  }
  Ok(())
}

#[test]
fn policy_holds_only_a_floor_of_at_least_80() -> Result<()> {
  assert_eq!(Policy::parse(r#"{"minimum_line_percent":85}"#)?.minimum, 85);
  for input in [
    "{}",
    "null",
    r#"{"minimum_line_percent":79}"#,
    r#"{"minimum_line_percent":101}"#,
    r#"{"minimum_line_percent":"80"}"#,
    r#"{"minimum_line_percent":80,"skip":true}"#,
  ] {
    assert!(Policy::parse(input).is_err(), "accepted {input}");
  }
  Ok(())
}

#[test]
fn a_committed_floor_cannot_be_lowered() -> Result<()> {
  // Committed policies may still hold the fields earlier versions recorded.
  let older = r#"{"minimum_line_percent":80,"baseline_covered_lines":45614,"baseline_instrumented_lines":55428}"#;
  Policy { minimum: 80 }.protect(Policy::committed_floor(older)?)?;
  Policy { minimum: 85 }.protect(80)?;
  assert!(Policy { minimum: 80 }.protect(85).is_err());
  assert!(Policy::committed_floor("{}").is_err());
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
