//! Parent-owned resource reporting. It reads only its own process and private worker events.
use flinch_video_engine::{
  EngineError, ErrorKind, Result,
  artifact::{self, Journal},
  metrics::resources::CombinedTracker,
  types::{OutcomeKind, RecordingOutcome, SessionSnapshot},
  windows::telemetry::ResourceSampler,
};
use std::path::{Path, PathBuf};

pub(crate) fn bounded_io<T: Send + 'static>(
  stage: &'static str,
  timeout: std::time::Duration,
  operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
  let (finished, result) = std::sync::mpsc::sync_channel(1);
  std::thread::Builder::new()
    .name("video lab metadata I/O".into())
    .spawn(move || {
      // One result fits without waiting; a timed-out owner never joins this potentially blocked I/O.
      let _ = finished.send(operation());
    })
    .map_err(|error| EngineError::io(stage, error))?;
  result.recv_timeout(timeout).map_err(|error| {
    EngineError::new(
      if matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout) {
        ErrorKind::Timeout
      } else {
        ErrorKind::Io
      },
      stage,
      error.to_string(),
    )
  })?
}

pub struct ResourceReports {
  sampler: ResourceSampler,
  tracker: CombinedTracker,
  journal: Option<Journal>,
  directory: Option<PathBuf>,
  controls: Option<flinch_video_engine::client::control::ControlReport>,
}
impl ResourceReports {
  pub fn new() -> Result<Self> {
    Ok(Self {
      sampler: ResourceSampler::new()?,
      tracker: CombinedTracker::default(),
      journal: None,
      directory: None,
      controls: None,
    })
  }
  pub fn prepared(&mut self, directory: &Path) -> Result<()> {
    if self.directory.is_some() {
      return Err(EngineError::protocol(
        "Supervisor session directory already confirmed",
      ));
    }
    self.directory = Some(directory.to_owned());
    let path = directory.join("supervisor-resources.jsonl");
    self.journal = Some(
      bounded_io(
        "supervisor journal prepare",
        flinch_video_engine::limits::CONTROL_TIMEOUT,
        move || Journal::open(&path),
      )
      .map_err(|mut error| {
        error.recoverable_path = Some(directory.to_owned());
        error
      })?,
    );
    Ok(())
  }
  pub fn controls(&mut self, report: flinch_video_engine::client::control::ControlReport) {
    self.controls = Some(report);
  }
  pub fn observe(&mut self, snapshot: &mut SessionSnapshot) -> Result<()> {
    let Some(worker) = snapshot.process_resources else {
      snapshot.combined_resources = None;
      return Ok(());
    };
    let (_, supervisor) = self.sampler.sample_detailed()?;
    let mut combined = self.tracker.observe(worker, supervisor)?;
    if let Some(journal) = &mut self.journal {
      combined.diagnostics_lost = journal.lost;
      journal.push(&combined)?;
      combined.diagnostics_lost = journal.lost;
    }
    snapshot.combined_resources = Some(combined);
    Ok(())
  }
  pub fn finish(&mut self, outcome: &RecordingOutcome) -> Result<()> {
    self.finish_before(
      outcome,
      std::time::Instant::now() + flinch_video_engine::limits::CONTROL_TIMEOUT,
    )
  }
  pub fn finish_before(
    &mut self,
    outcome: &RecordingOutcome,
    deadline: std::time::Instant,
  ) -> Result<()> {
    let directory = self.directory.clone();
    let outcome = outcome.clone();
    let journal = self.journal.take();
    let controls = self.controls;
    bounded_io(
      "supervisor report finalization",
      remaining(deadline, "supervisor report finalization")?
        .min(flinch_video_engine::limits::CONTROL_TIMEOUT),
      move || {
        // A slow diagnostic drive cannot block the supervision loop indefinitely.
        if let Some(directory) = directory {
          if let Some(controls) = controls {
            artifact::write_json(&directory.join("supervisor-controls.json"), &controls)?;
          }
          artifact::write_json(&directory.join("supervisor-result.json"), &outcome)?;
        }
        if let Some(mut journal) = journal {
          journal.finish()?;
        }
        Ok(())
      },
    )
  }
  pub fn failure(
    &mut self,
    mut error: EngineError,
    mut snapshot: SessionSnapshot,
    kind: OutcomeKind,
  ) -> RecordingOutcome {
    if error.recoverable_path.is_none() {
      error.recoverable_path = self
        .directory
        .as_ref()
        .map(|directory| directory.join("capture.partial.mp4"));
    }
    if let Err(diagnostic) = self.observe(&mut snapshot) {
      error
        .reason
        .push_str(&format!("; resource reporting: {diagnostic}"));
    }
    let partial = self
      .directory
      .as_ref()
      .map(|directory| directory.join("capture.partial.mp4"))
      .or_else(|| error.recoverable_path.clone()); // Candidate path; recover verifies existence and structure.
    let mut outcome = RecordingOutcome {
      thumbnail: None,
      kind,
      output: None,
      partial,
      error: Some(error.clone()),
      snapshot,
    };
    if let Err(diagnostic) = self.finish(&outcome) {
      error
        .reason
        .push_str(&format!("; supervisor report: {diagnostic}"));
    }
    outcome.error = Some(error);
    outcome
  }
}

pub(crate) fn remaining(deadline: std::time::Instant, stage: &str) -> Result<std::time::Duration> {
  deadline
    .checked_duration_since(std::time::Instant::now())
    .filter(|remaining| !remaining.is_zero())
    .ok_or_else(|| {
      EngineError::new(
        ErrorKind::Timeout,
        stage,
        "The shared operation deadline has expired",
      )
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  use flinch_video_engine::types::SessionState;
  #[test]
  fn an_expired_shared_budget_never_starts_another_manifest_or_console_wait() {
    let root = tempfile::tempdir().expect("temp");
    let mut reports = ResourceReports::new().expect("sampler");
    reports.prepared(root.path()).expect("session directory");
    let outcome = RecordingOutcome {
      thumbnail: None,
      kind: OutcomeKind::Aborted,
      output: None,
      partial: None,
      error: None,
      snapshot: SessionSnapshot {
        state: SessionState::Aborted,
        ..Default::default()
      },
    };
    let expired = std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert_eq!(
      reports
        .finish_before(&outcome, expired)
        .expect_err("no budget")
        .kind,
      ErrorKind::Timeout
    );
    assert!(!root.path().join("supervisor-result.json").exists());
    assert_eq!(
      crate::print_json_before(&outcome, expired)
        .expect_err("no fresh output budget")
        .kind,
      ErrorKind::Timeout
    );
  }
  #[test]
  fn metadata_that_does_not_return_cannot_block_supervision() {
    let (release, blocked) = std::sync::mpsc::sync_channel(1);
    let error = bounded_io(
      "scripted blocked I/O",
      std::time::Duration::from_millis(1),
      move || {
        blocked
          .recv_timeout(std::time::Duration::from_secs(2))
          .map_err(|error| EngineError::protocol(error.to_string()))
      },
    )
    .expect_err("finite metadata deadline");
    assert_eq!(error.kind, ErrorKind::Timeout);
    release.send(()).expect("release background fixture");
  }
  #[test]
  fn failure_preserves_its_native_cause_and_writes_only_inside_the_owned_session() {
    let directory = tempfile::tempdir().expect("session");
    let mut reports = ResourceReports::new().expect("own process sampler");
    reports.prepared(directory.path()).expect("owned directory");
    std::fs::write(
      directory.path().join("capture.partial.mp4"),
      b"retained partial",
    )
    .expect("partial");
    let original = EngineError {
      native_code: Some(123),
      ..EngineError::new(
        ErrorKind::Timeout,
        "scripted native call",
        "original failure",
      )
    };
    let error = reports
      .failure(
        original,
        SessionSnapshot {
          state: SessionState::Failed,
          ..Default::default()
        },
        OutcomeKind::Failed,
      )
      .error
      .expect("terminal cause");
    assert_eq!(error.stage, "scripted native call");
    assert_eq!(error.native_code, Some(123));
    assert_eq!(error.reason, "original failure");
    assert_eq!(
      error.recoverable_path,
      Some(directory.path().join("capture.partial.mp4"))
    );
    let outcome: RecordingOutcome = serde_json::from_slice(
      &std::fs::read(directory.path().join("supervisor-result.json")).expect("retained result"),
    )
    .expect("outcome");
    assert_eq!(outcome.error.expect("cause").native_code, Some(123));
    assert!(outcome.snapshot.combined_resources.is_none());
    assert!(reports.prepared(directory.path()).is_err());
  }
}
