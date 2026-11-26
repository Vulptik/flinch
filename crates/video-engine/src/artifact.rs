//! Same-volume staging with preservation of failed and interrupted sessions.
use crate::{EngineError, ErrorKind, Result, types::SessionSpec};
use serde::Serialize;
use std::{
  fs::{self, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
  time::{SystemTime, UNIX_EPOCH},
};

/// Name prefix of every staged session directory. Hosts that sweep temporary folders use it to
/// retain sessions a crash left behind.
pub const SESSION_DIRECTORY_PREFIX: &str = ".flinch-video-";

pub struct SessionArtifact {
  pub directory: PathBuf,
  pub partial: PathBuf,
  pub output: PathBuf,
}
enum JournalMessage {
  Line(Vec<u8>),
  Finish,
}
/// Bounded asynchronous diagnostic output. A slow log disk never blocks capture.
pub struct Journal {
  sender: std::sync::mpsc::SyncSender<JournalMessage>,
  result: std::sync::mpsc::Receiver<Result<()>>,
  terminal: Option<Result<()>>,
  pub lost: u64,
}
impl Journal {
  pub fn open(path: &Path) -> Result<Self> {
    let file = OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(path)
      .map_err(|e| EngineError::io("journal create", e))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(256);
    let (done, result) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
      .name("video diagnostics".into())
      .spawn(move || {
        let outcome = (|| {
          let mut output = std::io::BufWriter::new(file);
          while let Ok(message) = receiver.recv() {
            match message {
              JournalMessage::Line(line) => output
                .write_all(&line)
                .map_err(|e| EngineError::io("journal write", e))?,
              JournalMessage::Finish => {
                return output
                  .flush()
                  .map_err(|e| EngineError::io("journal flush", e));
              },
            }
          }
          output
            .flush()
            .map_err(|e| EngineError::io("journal disconnected flush", e))
        })();
        if let Err(disconnected) = done.send(outcome)
          && let Err(error) = disconnected.0
        {
          eprintln!("Diagnostic writer failed after owner closed: {error}");
        }
      })
      .map_err(|e| EngineError::io("journal thread", e))?;
    Ok(Self {
      sender,
      result,
      terminal: None,
      lost: 0,
    })
  }
  pub fn push(&mut self, value: &impl Serialize) -> Result<()> {
    if let Some(result) = &self.terminal {
      return match result {
        Err(error) => Err(error.clone()),
        Ok(()) => Err(EngineError::protocol("Journal is finalized")),
      };
    }
    if let Ok(result) = self.result.try_recv() {
      let error = result
        .err()
        .unwrap_or_else(|| EngineError::protocol("Journal exited before finalization"));
      self.terminal = Some(Err(error.clone()));
      return Err(error);
    }
    let mut line = crate::limits::json_record(value, 4095)?;
    line.reserve_exact(1);
    line.push(b'\n');
    match self.sender.try_send(JournalMessage::Line(line)) {
      Ok(()) => Ok(()),
      Err(std::sync::mpsc::TrySendError::Full(_)) => {
        self.lost = self.lost.saturating_add(1);
        Ok(())
      },
      Err(e) => {
        let error = EngineError::new(ErrorKind::Io, "journal disconnected", e.to_string());
        self.terminal = Some(Err(error.clone()));
        Err(error)
      },
    }
  }
  pub fn finish(&mut self) -> Result<()> {
    if let Some(result) = &self.terminal {
      return result.clone();
    }
    let result = (|| {
      self
        .sender
        .try_send(JournalMessage::Finish)
        .map_err(|e| EngineError::new(ErrorKind::Io, "journal finish", e.to_string()))?;
      self
        .result
        .recv_timeout(crate::limits::CONTROL_TIMEOUT)
        .map_err(|e| {
          EngineError::new(
            if matches!(e, std::sync::mpsc::RecvTimeoutError::Timeout) {
              ErrorKind::Timeout
            } else {
              ErrorKind::Io
            },
            "journal finish",
            e.to_string(),
          )
        })?
    })();
    self.terminal = Some(result.clone());
    result
  }
}
/// Move `from` to `to` on the same volume, failing if `to` exists. `std::fs::rename` replaces
/// an existing destination on Windows, so an existence check before it leaves a race in which
/// a file created meanwhile is silently overwritten; this move refuses atomically instead.
pub fn move_no_replace(from: &Path, to: &Path, stage: &str) -> Result<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
  let wide = |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain([0]).collect() };
  let (source, destination) = (wide(from), wide(to));
  // SAFETY: both paths are NUL-terminated and outlive the call; no replacement flag is passed.
  unsafe {
    MoveFileExW(
      windows::core::PCWSTR(source.as_ptr()),
      windows::core::PCWSTR(destination.as_ptr()),
      MOVEFILE_WRITE_THROUGH,
    )
  }
  .map_err(|error| EngineError::new(ErrorKind::Io, stage, error.to_string()))
}
pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
  let bytes = serde_json::to_vec_pretty(value).map_err(|e| EngineError::protocol(e.to_string()))?;
  let temporary = path.with_extension("json.tmp");
  let mut file = OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(&temporary)
    .map_err(|e| EngineError::io("manifest create", e))?;
  file
    .write_all(&bytes)
    .and_then(|_| file.sync_all())
    .map_err(|e| EngineError::io("manifest write", e))?;
  drop(file);
  fs::rename(&temporary, path).map_err(|e| EngineError::io("manifest publish", e))
}
impl SessionArtifact {
  pub fn create(spec: &SessionSpec) -> Result<Self> {
    spec.validate()?;
    let output = if spec.output.is_absolute() {
      spec.output.clone()
    } else {
      std::env::current_dir()
        .map_err(|e| EngineError::io("current directory", e))?
        .join(&spec.output)
    };
    if output.exists() {
      return Err(EngineError::new(
        ErrorKind::Io,
        "output",
        "Destination already exists",
      ));
    }
    let parent = output
      .parent()
      .ok_or_else(|| EngineError::protocol("Output has no parent"))?;
    fs::create_dir_all(parent).map_err(|e| EngineError::io("output directory", e))?;
    let nonce = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map_err(|e| EngineError::protocol(e.to_string()))?
      .as_nanos();
    let directory = parent.join(format!(
      "{SESSION_DIRECTORY_PREFIX}{}-{nonce}",
      std::process::id()
    ));
    fs::create_dir(&directory).map_err(|e| EngineError::io("session directory", e))?;
    let partial = directory.join("capture.partial.mp4");
    write_json(&directory.join("session.json"), spec)?;
    Ok(Self {
      directory,
      partial,
      output,
    })
  }
  pub fn publish(&self) -> Result<()> {
    if self.output.exists() {
      return Err(EngineError::new(
        ErrorKind::Io,
        "publish",
        "Destination was created during recording; partial retained",
      ));
    }
    OpenOptions::new()
      .read(true)
      .write(true)
      .open(&self.partial)
      .and_then(|f| f.sync_all())
      .map_err(|e| EngineError::io("flush output", e))?;
    // Atomic no-replace move: a destination created after the check above still wins.
    move_no_replace(&self.partial, &self.output, "publish")
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)] // Test fixtures only; production remains deny.
mod tests {
  use super::*;
  use crate::types::*;
  #[test]
  fn asynchronous_journal_flushes_without_touching_other_files() {
    let t = tempfile::tempdir().expect("fixture");
    let path = t.path().join("measurements.jsonl");
    let mut log = Journal::open(&path).expect("journal");
    for index in 0..5 {
      log
        .push(&serde_json::json!({"index":index}))
        .expect("record");
    }
    assert!(log.push(&"x".repeat(4096)).is_err());
    log.finish().expect("flush");
    log
      .finish()
      .expect("repeated finish observes the same completion");
    assert!(log.push(&serde_json::json!({"after_finish":true})).is_err());
    let text = std::fs::read_to_string(&path).expect("read");
    assert_eq!(text.lines().count(), 5);
    assert_eq!(log.lost, 0);
    assert!(Journal::open(&path).is_err());
  }
  #[test]
  fn consumed_diagnostic_failure_remains_observable_to_push_and_finish() {
    let (sender, _messages) = std::sync::mpsc::sync_channel(1);
    let (done, result) = std::sync::mpsc::sync_channel(1);
    let original = EngineError::io(
      "journal write",
      std::io::Error::from(std::io::ErrorKind::StorageFull),
    );
    done
      .send(Err(original.clone()))
      .expect("scripted disk error");
    let mut journal = Journal {
      sender,
      result,
      terminal: None,
      lost: 0,
    };
    assert_eq!(journal.push(&1).expect_err("first report"), original);
    assert_eq!(journal.push(&2).expect_err("error is retained"), original);
    assert_eq!(
      journal
        .finish()
        .expect_err("finish preserves write failure"),
      original
    );
  }
  #[test]
  fn concurrent_destination_and_failed_data_survive() {
    let t = tempfile::tempdir().expect("valid test fixture");
    let spec = SessionSpec {
      monitor: "primary".into(),
      fps: 60,
      capture: CaptureKind::Dxgi,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
      cursor: false,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: t.path().join("out.mp4"),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    };
    let a = SessionArtifact::create(&spec).expect("valid test fixture");
    fs::write(&a.partial, b"partial").expect("valid test fixture");
    fs::write(&spec.output, b"original").expect("valid test fixture");
    assert!(a.publish().is_err());
    assert_eq!(
      fs::read(&spec.output).expect("valid test fixture"),
      b"original"
    );
    assert!(a.partial.exists());
    assert!(SessionArtifact::create(&spec).is_err());
  }
  #[test]
  fn publication_never_replaces_a_destination_created_after_its_check() {
    let t = tempfile::tempdir().expect("valid test fixture");
    let (from, to) = (t.path().join("partial.mp4"), t.path().join("out.mp4"));
    fs::write(&from, b"new").expect("valid test fixture");
    fs::write(&to, b"someone else's").expect("valid test fixture");
    // The race: the destination exists by the time the move runs.
    let error = move_no_replace(&from, &to, "publish").expect_err("destination exists");
    assert_eq!(
      (error.kind, error.stage.as_str()),
      (ErrorKind::Io, "publish")
    );
    assert_eq!(fs::read(&to).expect("kept"), b"someone else's");
    assert_eq!(fs::read(&from).expect("kept"), b"new");
    fs::remove_file(&to).expect("valid test fixture");
    move_no_replace(&from, &to, "publish").expect("free destination");
    assert_eq!(fs::read(&to).expect("moved"), b"new");
    assert!(!from.exists());
    assert!(
      move_no_replace(&from, &to, "publish").is_err(),
      "missing source"
    );
  }
  #[test]
  fn publishes_same_volume_without_copying() {
    let t = tempfile::tempdir().expect("valid test fixture");
    let spec = SessionSpec {
      monitor: "primary".into(),
      fps: 60,
      capture: CaptureKind::Dxgi,
      encoder: EncoderKind::Amf,
      quality: Quality::default(),
      cursor: false,
      clicks: false,
      audio: false,
      audio_device: None,
      microphone: false,
      microphone_device: None,
      output: t.path().join("out.mp4"),
      trace_frames: false,
      region: None,
      span: Vec::new(),
    };
    let a = SessionArtifact::create(&spec).expect("valid test fixture");
    fs::write(&a.partial, b"media").expect("valid test fixture");
    a.publish().expect("valid test fixture");
    assert!(!a.partial.exists());
    assert_eq!(fs::read(&a.output).expect("valid test fixture"), b"media");
  }
}
