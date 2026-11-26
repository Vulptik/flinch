//! Lab-only run evidence, recorded after the terminal result so it never delays Prepare, Start or Stop.
use flinch_video_engine::{
  Result, artifact, limits,
  windows::{build_identity::HeldExecutable, calibration::Calibration},
};
use serde::Serialize;
use std::{path::Path, time::Instant};

#[derive(Serialize)]
struct Timing {
  phase: &'static str,
  build_identity_us: u64,
  aac_calibration_us: Option<u64>,
}

/// Writes `build.json`, `aac-calibration.json` (audio runs) and their costs to `provenance.json`.
/// A calibration outside its 1 ms contract fails the run's evidence instead of being omitted.
pub(crate) fn record(
  directory: &Path,
  executable: HeldExecutable,
  audio: bool,
  calibrate: impl FnOnce() -> Result<Calibration> + Send + 'static,
) -> Result<()> {
  let directory = directory.to_owned();
  crate::resources::bounded_io("lab provenance", limits::PREPARE_TIMEOUT, move || {
    let started = Instant::now();
    let identity = executable.identify()?;
    let build_identity_us = started.elapsed().as_micros() as u64;
    artifact::write_json(&directory.join("build.json"), &identity)?;
    let aac_calibration_us = if audio {
      let started = Instant::now();
      let calibration = calibrate()?;
      let elapsed = started.elapsed().as_micros() as u64;
      artifact::write_json(&directory.join("aac-calibration.json"), &calibration)?;
      Some(elapsed)
    } else {
      None
    };
    artifact::write_json(
      &directory.join("provenance.json"),
      &Timing {
        phase: "after_terminal_result",
        build_identity_us,
        aac_calibration_us,
      },
    )
  })
}

/// The recording's own failure stays primary; missing provenance alone still rejects the run.
pub(crate) fn merge(recording: Result<()>, provenance: Result<()>) -> Result<()> {
  match (recording, provenance) {
    (Ok(()), provenance) => provenance,
    (Err(original), Ok(())) => Err(original),
    (Err(mut original), Err(provenance)) => {
      original
        .reason
        .push_str(&format!("; lab provenance: {provenance}"));
      Err(original)
    },
  }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use flinch_video_engine::{EngineError, ErrorKind};
  use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  };

  fn calibration() -> Calibration {
    Calibration {
      input_frames: 72000,
      decoded_frames: 73024,
      encoded_packets: 72,
      first_packet_pts: 0,
      decoded_onset_frame: 24000,
      onset_error_frames: 0,
      decoded_padding_frames: 1024,
      timestamp_adjustment_frames: 0,
    }
  }
  fn executable(root: &Path) -> HeldExecutable {
    let path = root.join("worker-fixture.exe");
    std::fs::write(&path, b"abc").expect("binary fixture");
    HeldExecutable::open(&path).expect("held binary")
  }

  #[test]
  fn provenance_is_written_after_the_session_and_calibrates_only_audio_runs() {
    for audio in [true, false] {
      let root = tempfile::tempdir().expect("temp");
      let called = Arc::new(AtomicBool::new(false));
      let observed = called.clone();
      record(root.path(), executable(root.path()), audio, move || {
        observed.store(true, Ordering::Release);
        Ok(calibration())
      })
      .expect("software-only provenance");
      assert_eq!(called.load(Ordering::Acquire), audio);
      let build: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("build.json")).expect("build identity"),
      )
      .expect("JSON");
      assert_eq!(
        build["executable_sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
      );
      assert_eq!(root.path().join("aac-calibration.json").exists(), audio);
      let timing: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("provenance.json")).expect("timing"),
      )
      .expect("JSON");
      assert_eq!(timing["phase"], "after_terminal_result");
      assert!(timing["build_identity_us"].is_u64());
      assert_eq!(timing["aac_calibration_us"].is_u64(), audio);
    }
  }

  #[test]
  fn failed_provenance_rejects_the_run_without_replacing_a_recording_failure() {
    let root = tempfile::tempdir().expect("temp");
    let refused = record(root.path(), executable(root.path()), true, || {
      Err(EngineError::protocol(
        "AAC calibration delay 60 frames exceeds one millisecond",
      ))
    })
    .expect_err("calibration outside its contract");
    assert!(refused.reason.contains("exceeds one millisecond"));
    assert!(!root.path().join("aac-calibration.json").exists());
    assert!(!root.path().join("provenance.json").exists());

    let original = || {
      Err(EngineError {
        native_code: Some(91),
        ..EngineError::new(ErrorKind::Native, "encoder", "original cause")
      })
    };
    assert!(merge(Ok(()), Ok(())).is_ok());
    assert_eq!(
      merge(Ok(()), Err(refused.clone()))
        .expect_err("evidence without provenance")
        .reason,
      refused.reason
    );
    let kept = merge(original(), Ok(())).expect_err("recording failure");
    assert_eq!(
      (kept.stage.as_str(), kept.reason.as_str()),
      ("encoder", "original cause")
    );
    let both = merge(original(), Err(refused)).expect_err("both failed");
    assert_eq!(
      (both.stage.as_str(), both.native_code),
      ("encoder", Some(91))
    );
    assert!(both.reason.starts_with("original cause; lab provenance: "));
  }
}
