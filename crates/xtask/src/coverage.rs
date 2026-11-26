//! Fresh, production-only coverage. No report-import, skip-test or threshold override options.
use crate::{process, read, tasks};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
  collections::{BTreeMap, BTreeSet},
  fs,
  path::{Path, PathBuf},
};
use syn::{
  spanned::Spanned,
  visit::{self, Visit},
};

const POLICY: &str = "scripts/coverage-policy.json";
/// Set by `test:coverage` so this binary runs as Cargo's `RUSTC_WORKSPACE_WRAPPER`.
pub const WRAPPER_ENV: &str = "FLINCH_XTASK_COVERAGE_WRAPPER";
/// The flags cargo-llvm-cov's own wrapper adds to each workspace crate.
const COVERAGE_FLAGS: [&str; 2] = ["-Cinstrument-coverage", "--cfg=coverage"];
const ROOTS: &[&str] = &[
  "src",
  "crates/settings/src",
  "crates/installer/src",
  "crates/video-engine/src",
  "crates/video-lab/src",
  "windows",
];
const HARNESSES: &[&str] = &[
  "src/settings/native_probe.rs",
  "crates/settings/src/interaction_check.rs",
];

fn workspace_root() -> Result<PathBuf> {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .canonicalize()
    .context("Resolve coverage workspace root")
}

#[derive(Clone, Copy, Debug)]
struct Counts {
  covered: u64,
  lines: u64,
}
impl Counts {
  fn percent(self) -> f64 {
    self.covered as f64 * 100.0 / self.lines as f64
  }
  fn at_least(self, other: Self) -> bool {
    u128::from(self.covered) * u128::from(other.lines)
      >= u128::from(other.covered) * u128::from(self.lines)
  }
}

/// The coverage policy: one fixed floor. Coverage is measured fresh on whichever machine runs the
/// gate, so it is compared only with this fixed number, never with a result recorded elsewhere.
#[derive(Clone)]
struct Policy {
  minimum: u64,
}
impl Policy {
  fn parse(text: &str) -> Result<Self> {
    let value: serde_json::Value = serde_json::from_str(text).context("Parse coverage policy")?;
    let fields = value
      .as_object()
      .context("Coverage policy must be an object")?;
    ensure!(
      fields.len() == 1,
      "Coverage policy must hold only minimum_line_percent"
    );
    let minimum = value
      .get("minimum_line_percent")
      .and_then(serde_json::Value::as_u64)
      .context("Invalid coverage policy field: minimum_line_percent")?;
    ensure!(
      (80..=100).contains(&minimum),
      "The coverage floor must never be below 80%"
    );
    Ok(Self { minimum })
  }
  fn enforce(&self, current: Counts) -> Result<()> {
    ensure!(
      current.lines > 0 && current.covered <= current.lines,
      "Missing or invalid production coverage"
    );
    ensure!(
      current.at_least(Counts {
        covered: self.minimum,
        lines: 100
      }),
      "Coverage gate failed: {:.4}% ({}/{}) is below the {}% floor",
      current.percent(),
      current.covered,
      current.lines,
      self.minimum
    );
    Ok(())
  }
  /// The floor in a committed policy, whose file may still hold fields older versions used.
  fn committed_floor(text: &str) -> Result<u64> {
    serde_json::from_str::<serde_json::Value>(text)
      .context("Parse committed coverage policy")?
      .get("minimum_line_percent")
      .and_then(serde_json::Value::as_u64)
      .context("Committed coverage policy has no minimum_line_percent")
  }
  fn protect(&self, previous_floor: u64) -> Result<()> {
    ensure!(
      self.minimum >= previous_floor,
      "Coverage policy cannot lower a previously committed floor"
    );
    Ok(())
  }
}

fn protect_history(policy: &Policy) -> Result<()> {
  let revisions = process::capture(
    process::command("git").args(["rev-list", "--max-count=2", "HEAD"]),
    10,
  )?;
  let revisions: Vec<_> = revisions.lines().filter(|s| !s.is_empty()).collect();
  let shallow = process::capture(
    process::command("git").args(["rev-parse", "--is-shallow-repository"]),
    10,
  )?;
  ensure!(
    revisions.len() >= 2 || shallow.trim() == "false",
    "Coverage policy checks require checkout fetch-depth: 2 or deeper"
  );
  for revision in revisions {
    let entry = process::capture(
      process::command("git").args(["ls-tree", "--name-only", revision, "--", POLICY]),
      10,
    )?;
    if !entry.trim().is_empty() {
      let previous = process::capture(
        process::command("git").args(["show", &format!("{revision}:{POLICY}")]),
        10,
      )?;
      policy.protect(Policy::committed_floor(&previous)?)?;
    }
  }
  Ok(())
}

fn requires_test(meta: &syn::Meta) -> bool {
  match meta {
    syn::Meta::Path(p) => p.is_ident("test"),
    syn::Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
      let nested = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
      );
      nested.is_ok_and(|items| {
        !items.is_empty()
          && if list.path.is_ident("all") {
            items.iter().any(requires_test)
          } else {
            items.iter().all(requires_test)
          }
      })
    },
    _ => false,
  }
}
fn test_only(attrs: &[syn::Attribute]) -> bool {
  attrs.iter().any(|a| {
    a.path().segments.last().is_some_and(|s| s.ident == "test")
      || (a.path().is_ident("cfg") && a.parse_args::<syn::Meta>().is_ok_and(|m| requires_test(&m)))
  })
}
#[derive(Default)]
struct Mask {
  ranges: Vec<(usize, usize)>,
  external: Vec<PathBuf>,
  module_dir: PathBuf,
  file_dir: PathBuf,
  inside_test: bool,
}
impl Mask {
  fn exclude(&mut self, span: proc_macro2::Span) {
    self.ranges.push((span.start().line, span.end().line));
  }
  fn contains(&self, line: usize) -> bool {
    self.ranges.iter().any(|&(a, b)| (a..=b).contains(&line))
  }
}
impl<'a> Visit<'a> for Mask {
  fn visit_item_mod(&mut self, item: &'a syn::ItemMod) {
    let previous_test = self.inside_test;
    self.inside_test |= test_only(&item.attrs);
    if self.inside_test {
      self.exclude(item.span());
    }
    if let Some((_, items)) = &item.content {
      let previous_dir = self.module_dir.clone();
      self.module_dir.push(item.ident.to_string());
      for item in items {
        self.visit_item(item);
      }
      self.module_dir = previous_dir;
    } else if self.inside_test {
      let explicit = item.attrs.iter().find_map(|a| {
        if !a.path().is_ident("path") {
          return None;
        }
        if let syn::Meta::NameValue(v) = &a.meta
          && let syn::Expr::Lit(e) = &v.value
          && let syn::Lit::Str(s) = &e.lit
        {
          return Some(s.value());
        }
        None
      });
      if let Some(path) = explicit {
        self.external.push(self.file_dir.join(path));
      } else {
        self
          .external
          .push(self.module_dir.join(format!("{}.rs", item.ident)));
        self
          .external
          .push(self.module_dir.join(item.ident.to_string()));
      }
    }
    self.inside_test = previous_test;
  }
  fn visit_item(&mut self, item: &'a syn::Item) {
    let attrs: &[syn::Attribute] = match item {
      syn::Item::Fn(i) => &i.attrs,
      syn::Item::Struct(i) => &i.attrs,
      syn::Item::Enum(i) => &i.attrs,
      syn::Item::Impl(i) => &i.attrs,
      syn::Item::Use(i) => &i.attrs,
      syn::Item::Const(i) => &i.attrs,
      syn::Item::Static(i) => &i.attrs,
      syn::Item::Type(i) => &i.attrs,
      syn::Item::Trait(i) => &i.attrs,
      _ => &[],
    };
    if self.inside_test || test_only(attrs) {
      self.exclude(item.span());
    }
    visit::visit_item(self, item);
  }
  fn visit_impl_item_fn(&mut self, item: &'a syn::ImplItemFn) {
    if test_only(&item.attrs) {
      self.exclude(item.span());
    } else {
      visit::visit_impl_item_fn(self, item);
    }
  }
  fn visit_expr_if(&mut self, item: &'a syn::ExprIf) {
    if test_only(&item.attrs) {
      self.exclude(item.span());
    } else {
      visit::visit_expr_if(self, item);
    }
  }
}

fn source_files(dir: &Path, files: &mut BTreeMap<PathBuf, String>) -> Result<()> {
  for entry in
    fs::read_dir(dir).with_context(|| format!("Read source directory {}", dir.display()))?
  {
    let path = entry?.path();
    if path.is_dir() {
      source_files(&path, files)?;
    } else if path.extension().is_some_and(|e| e == "rs") {
      files.insert(path.canonicalize()?, read(&path)?);
    }
  }
  Ok(())
}
fn sources() -> Result<BTreeMap<PathBuf, String>> {
  let mut files = BTreeMap::new();
  for root in ROOTS {
    source_files(Path::new(root), &mut files)?;
  }
  Ok(files)
}
fn masks(
  files: &BTreeMap<PathBuf, String>,
) -> Result<(BTreeMap<PathBuf, Mask>, BTreeSet<PathBuf>)> {
  let mut masks = BTreeMap::new();
  let mut excluded = BTreeSet::new();
  for harness in HARNESSES {
    excluded.insert(workspace_root()?.join(harness).canonicalize()?);
  }
  for (path, text) in files {
    let parent = path.parent().context("Source parent")?;
    let stem = path
      .file_stem()
      .context("Source file stem")?
      .to_string_lossy();
    let module_dir = if ["lib", "mod", "main"].contains(&stem.as_ref()) {
      parent.to_owned()
    } else {
      parent.join(stem.as_ref())
    };
    let mut mask = Mask {
      module_dir,
      file_dir: parent.to_owned(),
      ..Default::default()
    };
    mask.visit_file(
      &syn::parse_file(text).with_context(|| format!("Parse source {}", path.display()))?,
    );
    for external in &mask.external {
      if external.exists() {
        excluded.insert(external.canonicalize()?);
      }
    }
    masks.insert(path.clone(), mask);
  }
  Ok((masks, excluded))
}

fn measure(
  lcov: &str,
  files: &BTreeMap<PathBuf, String>,
  roots: &[PathBuf],
) -> Result<(Counts, serde_json::Value)> {
  let (masks, excluded) = masks(files)?;
  let mut hits: BTreeMap<PathBuf, BTreeMap<usize, u64>> = BTreeMap::new();
  let mut current: Option<PathBuf> = None;
  let mut in_record = false;
  for row in lcov.lines() {
    if let Some(file) = row.strip_prefix("SF:") {
      ensure!(!in_record && !file.is_empty(), "Invalid LCOV source record");
      in_record = true;
      let path = Path::new(file)
        .canonicalize()
        .with_context(|| format!("Resolve LCOV source {file}"))?;
      current = (files.contains_key(&path) && !excluded.iter().any(|p| path.starts_with(p)))
        .then_some(path);
    } else if let Some(data) = row.strip_prefix("DA:") {
      ensure!(in_record, "LCOV line outside a source record");
      if let Some(path) = &current {
        let mut values = data.split(',');
        let line: usize = values
          .next()
          .context("LCOV line number")?
          .parse()
          .context("Invalid LCOV line number")?;
        let count: u64 = values
          .next()
          .context("LCOV hit count")?
          .parse()
          .context("Invalid LCOV hit count")?;
        let source = files.get(path).context("LCOV source missing")?;
        ensure!(
          line > 0 && line <= source.lines().count(),
          "LCOV line outside current source: {}:{line}",
          path.display()
        );
        if !masks
          .get(path)
          .context("Source mask missing")?
          .contains(line)
        {
          let existing = hits
            .entry(path.clone())
            .or_default()
            .entry(line)
            .or_default();
          *existing = (*existing).max(count);
        }
      }
    } else if row == "end_of_record" {
      ensure!(in_record, "LCOV record ended without a source");
      in_record = false;
      current = None;
    }
  }
  ensure!(!in_record, "Truncated LCOV source record");
  // An uninstrumented crate leaves no LCOV records, so its lines would silently fall out of
  // the denominator instead of counting as uncovered. Every production root must report.
  for root in roots {
    let expected = files
      .keys()
      .any(|path| path.starts_with(root) && !excluded.iter().any(|e| path.starts_with(e)));
    ensure!(
      !expected || hits.keys().any(|path| path.starts_with(root)),
      "No coverage data for production sources under {}; their crate was not instrumented",
      root.display()
    );
  }
  let mut total = Counts {
    covered: 0,
    lines: 0,
  };
  let root = workspace_root()?;
  let mut rows = Vec::new();
  for (path, lines) in hits {
    let counts = Counts {
      covered: lines.values().filter(|&&n| n > 0).count() as u64,
      lines: lines.len() as u64,
    };
    total.covered += counts.covered;
    total.lines += counts.lines;
    rows.push(json!({ "path": path.strip_prefix(&root)?.to_string_lossy().replace('\\', "/"), "covered": counts.covered, "lines": counts.lines }));
  }
  ensure!(total.lines > 0, "No production Rust line coverage found");
  Ok((
    total,
    json!({"scope": "Production Rust under src, crates/settings/src, crates/installer/src, crates/video-engine/src, crates/video-lab/src and windows; cfg(test) items and external test modules excluded; the real worker suite runs against an instrumented flinch.exe; native settings probe and interaction registry excluded. Tooling, vendor, generated, integration-test and build-script sources excluded. Feature-disabled code and non-Rust sources are not measured; desktop acceptance is separate.", "covered": total.covered, "lines": total.lines, "line_percent": total.percent(), "files": rows}),
  ))
}

/// Whether one workspace rustc invocation still lacks coverage instrumentation. cargo-llvm-cov
/// 0.9.1's wrapper looks for `--crate-name` on the command line only; when Cargo moves a long
/// command line into an `@argfile` (`flinch`, `flinch-settings`), that crate is passed through
/// uninstrumented. Cargo's `___` target-info probe is not a crate build.
fn needs_coverage_flags(args: &[String], argfiles: &[String]) -> bool {
  let all: Vec<&str> = args
    .iter()
    .map(String::as_str)
    .chain(argfiles.iter().flat_map(|text| text.lines()))
    .collect();
  let crate_name = all
    .windows(2)
    .find(|pair| pair[0] == "--crate-name")
    .map(|pair| pair[1]);
  matches!(crate_name, Some(name) if name != "___")
    && !all.iter().any(|arg| arg.contains("instrument-coverage"))
}

/// Runs rustc for a workspace member, adding the coverage flags cargo-llvm-cov missed.
/// Cargo invokes it as `<wrapper> <rustc> <args...>` and only for workspace members, so
/// dependencies stay uninstrumented. Returns rustc's exit code.
pub fn rustc_wrapper(args: &[String]) -> Result<i32> {
  let (rustc, rest) = args
    .split_first()
    .context("The coverage rustc wrapper needs the rustc path")?;
  let argfiles = rest
    .iter()
    .filter_map(|arg| arg.strip_prefix('@'))
    .map(|path| fs::read_to_string(path).with_context(|| format!("Read rustc argfile {path}")))
    .collect::<Result<Vec<_>>>()?;
  let mut command = std::process::Command::new(rustc);
  command.args(rest);
  if needs_coverage_flags(rest, &argfiles) {
    command.args(COVERAGE_FLAGS);
  }
  let status = command.status().with_context(|| format!("Run {rustc}"))?;
  status
    .code()
    .with_context(|| format!("{rustc} ended without an exit code"))
}

pub fn run() -> Result<()> {
  let policy_text = read(POLICY)?;
  let policy = Policy::parse(&policy_text)?;
  protect_history(&policy)?;
  let before = sources()?;
  process::cargo(&["llvm-cov", "--version"])
    .context("Install cargo-llvm-cov 0.9.1 and rustup component llvm-tools-preview")?;
  let output = Path::new("target/coverage");
  fs::create_dir_all(output)?;
  for name in ["raw.lcov", "summary.json"] {
    let path = output.join(name);
    if path.exists() {
      fs::remove_file(path)?;
    }
  }
  process::cargo(&["llvm-cov", "clean", "--workspace"])?;
  // `desktop-app` only gates the app binary: the integration suites make Cargo build an
  // instrumented `flinch.exe` beside the tests, which the real worker suite then launches.
  // This binary also runs as RUSTC_WORKSPACE_WRAPPER (see `rustc_wrapper`) so workspace crates
  // whose rustc command line Cargo moved into an @argfile are still instrumented. Passing the
  // flags through RUSTFLAGS instead would instrument every dependency: the OCR engine then
  // slows past its time limit and the library suite takes about 25 times longer.
  let wrapper = std::env::current_exe().context("Locate xtask for the coverage rustc wrapper")?;
  let base = [
    "llvm-cov",
    "--no-report",
    "--workspace",
    "--no-default-features",
    "--features",
    "native-settings,desktop-app,flinch-installer/native-instance",
    "--lib",
  ];
  let mut args: Vec<String> = base.map(String::from).to_vec();
  args.extend(["--bin".into(), "xtask".into()]);
  for suite in tasks::QUIET {
    args.extend(["--test".into(), (*suite).into()]);
  }
  args.extend(["--", "--test-threads=1", "--quiet"].map(String::from));
  process::run(
    process::command("cargo")
      .args(args)
      .env("RUSTC_WORKSPACE_WRAPPER", &wrapper)
      .env(WRAPPER_ENV, "1"),
    1800,
  )?;
  // The recorder against that real worker with synthetic sources (quiet: no capture devices),
  // so the recording path is measured end to end; worker processes write their own profiles.
  let mut worker: Vec<String> = base.map(String::from).to_vec();
  worker.extend(
    [
      "--",
      tasks::NATIVE_RECORDING_FILTER,
      "--ignored",
      "--test-threads=1",
      "--quiet",
    ]
    .map(String::from),
  );
  process::run(
    process::command("cargo")
      .args(worker)
      .env("RUSTC_WORKSPACE_WRAPPER", &wrapper)
      .env(WRAPPER_ENV, "1"),
    1800,
  )?;
  process::cargo(&[
    "llvm-cov",
    "report",
    "--workspace",
    "--lcov",
    "--output-path",
    "target/coverage/raw.lcov",
  ])?;
  ensure!(
    before == sources()? && policy_text == read(POLICY)?,
    "Sources or coverage policy changed during measurement; rerun the coverage gate"
  );
  let roots = ROOTS
    .iter()
    .map(|root| {
      Path::new(root)
        .canonicalize()
        .with_context(|| format!("Resolve coverage root {root}"))
    })
    .collect::<Result<Vec<_>>>()?;
  let (counts, mut report) = measure(&read(output.join("raw.lcov"))?, &before, &roots)?;
  report["minimum_line_percent"] = policy.minimum.into();
  fs::write(
    output.join("summary.json"),
    serde_json::to_string_pretty(&report)? + "\n",
  )?;
  println!(
    "Production Rust line coverage: {:.4}% ({}/{}); floor {}%. Evidence: target/coverage/summary.json",
    counts.percent(),
    counts.covered,
    counts.lines,
    policy.minimum
  );
  policy.enforce(counts)?;
  Ok(())
}

pub fn policy_contract() -> Result<()> {
  protect_history(&Policy::parse(&read(POLICY)?)?)?;
  let build = read(".github/workflows/build.yml")?;
  let release = read(".github/workflows/release.yml")?;
  for (name, workflow) in [("Build", build), ("Release", release)] {
    ensure!(
      workflow.contains("run: cargo xtask test:coverage")
        && !workflow.contains("continue-on-error"),
      "{name} CI must enforce coverage without allowing failure"
    );
  }
  Ok(())
}

#[cfg(test)]
#[path = "coverage_tests.rs"]
mod tests;
