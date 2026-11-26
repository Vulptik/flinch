//! `reencode REFERENCE.mp4 OUTPUT_DIR --encoder amf|nvenc|qsv|software (--qp LIST | --bitrate LIST)
//! [--warp]`: one reference recording through one encoder at several qualities, each scored
//! against the reference (see `flinch_video_engine::windows::reencode`). Offscreen; no desktop,
//! audio, window or input.
use crate::console;
use flinch_video_engine::{
  EngineError, Result,
  types::{EncoderKind, Quality},
  windows::reencode::{self, AdapterChoice},
};
use std::{
  ffi::OsString,
  path::{Path, PathBuf},
};

/// Qualities one invocation may measure.
const MAX_RUNS: usize = 16;

#[derive(Debug, PartialEq)]
pub struct Plan {
  pub input: PathBuf,
  pub directory: PathBuf,
  pub encoder: EncoderKind,
  pub qualities: Vec<Quality>,
  pub adapter: AdapterChoice,
}

fn list<T: std::str::FromStr + PartialEq>(text: &str, what: &str) -> Result<Vec<T>> {
  let values = text
    .split(',')
    .map(|part| part.trim().parse::<T>())
    .collect::<std::result::Result<Vec<_>, _>>()
    .map_err(|_| EngineError::protocol(format!("{what} must be a comma-separated list")))?;
  if values.is_empty() || values.len() > MAX_RUNS {
    return Err(EngineError::protocol(format!(
      "{what} takes 1 to {MAX_RUNS} values"
    )));
  }
  if values
    .iter()
    .enumerate()
    .any(|(index, value)| values[..index].contains(value))
  {
    return Err(EngineError::protocol(format!("{what} repeats a value")));
  }
  Ok(values)
}

pub fn plan(args: &[OsString]) -> Result<Plan> {
  let [input, directory, rest @ ..] = args else {
    return Err(EngineError::protocol(
      "reencode requires REFERENCE.mp4 and OUTPUT_DIR",
    ));
  };
  let mut encoder = None;
  let mut qualities: Option<Vec<Quality>> = None;
  let mut adapter = AdapterChoice::Display;
  let mut index = 0;
  while index < rest.len() {
    let name = rest[index]
      .to_str()
      .ok_or_else(|| EngineError::protocol("Option is not UTF-8"))?;
    if name == "--warp" {
      adapter = AdapterChoice::Warp;
      index += 1;
      continue;
    }
    let value = rest
      .get(index + 1)
      .and_then(|value| value.to_str())
      .ok_or_else(|| EngineError::protocol(format!("Missing value for {name}")))?;
    match name {
      "--encoder" => {
        encoder = Some(match value {
          "amf" => EncoderKind::Amf,
          "nvenc" => EncoderKind::Nvenc,
          "qsv" => EncoderKind::Qsv,
          "software" => EncoderKind::Software,
          _ => {
            return Err(EngineError::protocol(
              "reencode --encoder must be amf, nvenc, qsv or software",
            ));
          },
        })
      },
      "--qp" | "--bitrate" if qualities.is_some() => {
        return Err(EngineError::protocol("Use either --qp or --bitrate, once"));
      },
      "--qp" => {
        let values = list::<u8>(value, "--qp")?;
        if values.iter().any(|qp| *qp > 51) {
          return Err(EngineError::protocol("QP must be in 0..=51"));
        }
        qualities = Some(
          values
            .into_iter()
            .map(|qp| Quality::Cqp { i: qp, p: qp })
            .collect(),
        );
      },
      "--bitrate" => {
        qualities = Some(
          list::<u32>(value, "--bitrate")?
            .into_iter()
            .map(|bitrate| Quality::Vbr { bitrate })
            .collect(),
        );
      },
      _ => return Err(EngineError::protocol(format!("Unknown option {name}"))),
    }
    index += 2;
  }
  let encoder =
    encoder.ok_or_else(|| EngineError::protocol("reencode requires an explicit --encoder"))?;
  let qualities =
    qualities.ok_or_else(|| EngineError::protocol("reencode requires --qp or --bitrate"))?;
  if adapter == AdapterChoice::Warp && encoder != EncoderKind::Software {
    return Err(EngineError::protocol(
      "--warp records only with --encoder software",
    ));
  }
  Ok(Plan {
    input: PathBuf::from(input),
    directory: PathBuf::from(directory),
    encoder,
    qualities,
    adapter,
  })
}

/// `reference.amf.qp23.mp4`, `reference.software.warp.vbr8000000.mp4`.
pub fn output_name(plan: &Plan, quality: Quality) -> Result<PathBuf> {
  let stem = plan
    .input
    .file_stem()
    .ok_or_else(|| EngineError::protocol("Reference has no file name"))?
    .to_string_lossy();
  let encoder = match plan.encoder {
    EncoderKind::Amf => "amf",
    EncoderKind::Nvenc => "nvenc",
    EncoderKind::Qsv => "qsv",
    _ => "software",
  };
  let adapter = match plan.adapter {
    AdapterChoice::Warp => ".warp",
    AdapterChoice::Display => "",
  };
  let quality = match quality {
    Quality::Cqp { i, p } if i == p => format!("qp{i}"),
    Quality::Cqp { i, p } => format!("qp{i}-{p}"),
    Quality::Vbr { bitrate } => format!("vbr{bitrate}"),
  };
  Ok(
    plan
      .directory
      .join(format!("{stem}.{encoder}{adapter}.{quality}.mp4")),
  )
}

pub fn command(args: &[OsString]) -> Result<()> {
  let plan = plan(args)?;
  std::fs::create_dir_all(&plan.directory)
    .map_err(|error| EngineError::io("reencode directory", error))?;
  let runs = execute(&plan, |input, output, quality| {
    reencode::reencode(input, output, plan.encoder, quality, plan.adapter).and_then(|report| {
      serde_json::to_value(report).map_err(|e| EngineError::protocol(e.to_string()))
    })
  })?;
  let failed = runs.iter().filter(|run| run.get("error").is_some()).count();
  crate::print_json(&serde_json::json!({
    "reference": plan.input,
    "encoder": plan.encoder,
    "adapter": match plan.adapter { AdapterChoice::Warp => "warp", AdapterChoice::Display => "display" },
    "runs": runs,
  }))?;
  if failed > 0 {
    return Err(EngineError::protocol(format!(
      "{failed} of {} re-encodes failed; see their errors in the report",
      plan.qualities.len()
    )));
  }
  Ok(())
}

/// Every quality runs even after one fails; each failure stays in its own entry.
fn execute(
  plan: &Plan,
  mut run: impl FnMut(&Path, &Path, Quality) -> Result<serde_json::Value>,
) -> Result<Vec<serde_json::Value>> {
  let mut runs = Vec::with_capacity(plan.qualities.len());
  for (index, quality) in plan.qualities.iter().enumerate() {
    let output = output_name(plan, *quality)?;
    console::diagnostic(format!(
      "reencode {}/{}: {quality:?} -> {}",
      index + 1,
      plan.qualities.len(),
      output.display()
    ));
    runs.push(match run(&plan.input, &output, *quality) {
      Ok(report) => report,
      Err(error) => {
        console::diagnostic(format!("reencode {quality:?}: {error}"));
        serde_json::json!({"quality": quality, "output": output, "error": error})
      },
    });
  }
  Ok(runs)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
  }
  #[test]
  fn plans_need_a_reference_a_folder_an_explicit_encoder_and_one_quality_list() {
    let parsed = plan(&args(&[
      "ref.mp4",
      "out",
      "--encoder",
      "amf",
      "--qp",
      "18, 22,26",
    ]))
    .expect("plan");
    assert_eq!(parsed.input, PathBuf::from("ref.mp4"));
    assert_eq!(parsed.directory, PathBuf::from("out"));
    assert_eq!(parsed.encoder, EncoderKind::Amf);
    assert_eq!(parsed.adapter, AdapterChoice::Display);
    assert_eq!(
      parsed.qualities,
      [18, 22, 26].map(|qp| Quality::Cqp { i: qp, p: qp })
    );
    let vbr = plan(&args(&[
      "ref.mp4",
      "out",
      "--warp",
      "--bitrate",
      "4000000,8000000",
      "--encoder",
      "software",
    ]))
    .expect("vbr");
    assert_eq!(vbr.adapter, AdapterChoice::Warp);
    assert_eq!(vbr.qualities[1], Quality::Vbr { bitrate: 8_000_000 });
    for refused in [
      &["ref.mp4"][..],
      &["ref.mp4", "out", "--qp", "23"],
      &["ref.mp4", "out", "--encoder", "amf"],
      &["ref.mp4", "out", "--encoder", "auto", "--qp", "23"],
      &["ref.mp4", "out", "--encoder", "mf", "--qp", "23"],
      &["ref.mp4", "out", "--encoder", "amf", "--qp", "52"],
      &["ref.mp4", "out", "--encoder", "amf", "--qp", "23,23"],
      &["ref.mp4", "out", "--encoder", "amf", "--qp", "x"],
      &["ref.mp4", "out", "--encoder", "amf", "--qp", ""],
      &[
        "ref.mp4",
        "out",
        "--encoder",
        "amf",
        "--qp",
        "23",
        "--bitrate",
        "9",
      ],
      &["ref.mp4", "out", "--encoder", "amf", "--qp"],
      &["ref.mp4", "out", "--encoder", "amf", "--qp", "23", "--warp"],
      &[
        "ref.mp4",
        "out",
        "--encoder",
        "amf",
        "--qp",
        "23",
        "--fps",
        "30",
      ],
      &[
        "ref.mp4",
        "out",
        "--encoder",
        "amf",
        "--qp",
        "1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17",
      ],
    ] {
      assert!(plan(&args(refused)).is_err(), "{refused:?}");
    }
  }
  #[test]
  fn outputs_are_named_by_reference_encoder_adapter_and_quality() {
    let mut parsed = plan(&args(&[
      "clips/desk.mp4",
      "out",
      "--encoder",
      "nvenc",
      "--qp",
      "23",
    ]))
    .expect("plan");
    assert_eq!(
      output_name(&parsed, Quality::Cqp { i: 23, p: 23 }).expect("name"),
      PathBuf::from("out").join("desk.nvenc.qp23.mp4")
    );
    assert_eq!(
      output_name(&parsed, Quality::Cqp { i: 21, p: 23 }).expect("name"),
      PathBuf::from("out").join("desk.nvenc.qp21-23.mp4")
    );
    parsed.encoder = EncoderKind::Software;
    parsed.adapter = AdapterChoice::Warp;
    assert_eq!(
      output_name(&parsed, Quality::Vbr { bitrate: 8_000_000 }).expect("name"),
      PathBuf::from("out").join("desk.software.warp.vbr8000000.mp4")
    );
  }
  #[test]
  fn every_quality_runs_and_a_failure_stays_in_its_own_entry() {
    let parsed = plan(&args(&[
      "r.mp4",
      "o",
      "--encoder",
      "qsv",
      "--qp",
      "20,25,30",
    ]))
    .expect("plan");
    let mut seen = Vec::new();
    let runs = execute(&parsed, |_, output, quality| {
      seen.push(output.to_path_buf());
      match quality {
        Quality::Cqp { i: 25, .. } => Err(EngineError::protocol("encoder refused")),
        _ => Ok(serde_json::json!({"ok": true})),
      }
    })
    .expect("runs");
    assert_eq!(seen.len(), 3);
    assert_eq!(runs[0]["ok"], true);
    assert_eq!(runs[2]["ok"], true);
    assert_eq!(runs[1]["error"]["reason"], "encoder refused");
    assert!(
      runs[1]["output"]
        .as_str()
        .expect("output")
        .ends_with("r.qsv.qp25.mp4")
    );
  }
}
