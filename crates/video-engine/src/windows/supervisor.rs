//! Only the child we create is attached to the job. No PID or process-name discovery.
use crate::{
  EngineError, ErrorKind, Result,
  protocol::{self, Request, Response},
};
use std::{
  io::{BufReader, Read},
  os::windows::{
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::CommandExt,
  },
  process::{Child, Command, Stdio},
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender},
  },
  time::{Duration, Instant},
};
use windows::Win32::{Foundation::HANDLE, System::JobObjects::*};

static INTERRUPTS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
unsafe extern "system" fn console_control(kind: u32) -> windows::core::BOOL {
  if kind == 0 || kind == 1 {
    INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    true.into()
  } else {
    false.into()
  }
}
pub struct ConsoleSignals;
impl ConsoleSignals {
  pub fn install() -> Result<Self> {
    INTERRUPTS.store(0, std::sync::atomic::Ordering::Release);
    // SAFETY: static callback touches only an atomic; this is a process-local console handler.
    unsafe { windows::Win32::System::Console::SetConsoleCtrlHandler(Some(console_control), true) }
      .map_err(|e| EngineError::native("console control handler", e))?;
    Ok(Self)
  }
  pub fn take(&self) -> u8 {
    INTERRUPTS.swap(0, std::sync::atomic::Ordering::AcqRel)
  }
}
impl Drop for ConsoleSignals {
  fn drop(&mut self) {
    // SAFETY: unregisters exactly our own static callback, without changing other handlers.
    if let Err(e) = unsafe {
      windows::Win32::System::Console::SetConsoleCtrlHandler(Some(console_control), false)
    } {
      eprintln!("Console handler cleanup: {e}");
    }
  }
}

pub struct WorkerProcess {
  child: Child,
  input: Option<SyncSender<Request>>,
  events: Receiver<Result<Response>>,
  diagnostics: Receiver<String>,
  diagnostics_lost: Arc<AtomicU64>,
  _job: OwnedHandle,
}
impl WorkerProcess {
  pub fn spawn(executable: &std::path::Path) -> Result<Self> {
    Self::spawn_arguments(executable, &["--worker"])
  }
  pub fn spawn_verifier(
    executable: &std::path::Path,
    input: &std::path::Path,
    report: &std::path::Path,
    markers: bool,
  ) -> Result<Self> {
    let input = input
      .to_str()
      .ok_or_else(|| EngineError::protocol("Verification input is not UTF-8"))?;
    let report = report
      .to_str()
      .ok_or_else(|| EngineError::protocol("Verification report is not UTF-8"))?;
    Self::spawn_arguments(
      executable,
      &[
        "--verify-worker",
        input,
        report,
        if markers { "--markers" } else { "--decode" },
      ],
    )
  }
  pub fn spawn_fixture(
    executable: &std::path::Path,
    duration: &str,
    report: &std::path::Path,
    title: &str,
    monitor: &str,
  ) -> Result<Self> {
    let report = report
      .to_str()
      .ok_or_else(|| EngineError::protocol("Fixture report path is not UTF-8"))?;
    Self::spawn_arguments(
      executable,
      &["--fixture-worker", duration, report, title, monitor],
    )
  }
  /// Launch an engine worker through an application's own executable and worker switch.
  pub fn spawn_with(executable: &std::path::Path, args: &[&str]) -> Result<Self> {
    Self::spawn_arguments(executable, args)
  }
  fn spawn_arguments(executable: &std::path::Path, args: &[&str]) -> Result<Self> {
    let mut child = Command::new(executable)
      .args(args)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .creation_flags(0x08000000)
      .spawn()
      .map_err(|e| EngineError::io("worker spawn", e))?;
    let job = match attach(&child) {
      Ok(v) => v,
      Err(e) => {
        if let Err(kill) = child.kill() {
          eprintln!("Unattached worker cleanup: {kill}");
        }
        return Err(e);
      },
    };
    let mut pipe = child
      .stdin
      .take()
      .ok_or_else(|| EngineError::protocol("No worker input pipe"))?;
    let output = child
      .stdout
      .take()
      .ok_or_else(|| EngineError::protocol("No worker output pipe"))?;
    let (send, events) = mpsc::sync_channel(128);
    let mut stderr = child
      .stderr
      .take()
      .ok_or_else(|| EngineError::protocol("No worker diagnostics pipe"))?;
    let (logs, diagnostics) = mpsc::sync_channel(64);
    let diagnostics_lost = Arc::new(AtomicU64::new(0));
    let lost = diagnostics_lost.clone();
    std::thread::Builder::new()
      .name("video worker diagnostics".into())
      .spawn(move || {
        let mut bytes = [0u8; 4096];
        let mut lines = DiagnosticLines::default();
        let send = |message: String| {
          if logs.try_send(message).is_err() {
            lost.fetch_add(1, Ordering::Relaxed);
          }
        };
        loop {
          match stderr.read(&mut bytes) {
            Ok(0) => break,
            Ok(length) => lines.push(&bytes[..length]).into_iter().for_each(send),
            Err(error) => {
              lines.finish().into_iter().for_each(send);
              send(format!("Worker diagnostic pipe: {error}"));
              return;
            },
          }
        }
        lines.finish().into_iter().for_each(send);
      })
      .map_err(|e| EngineError::io("worker diagnostic reader", e))?;
    let errors = send.clone();
    let (input, commands) = mpsc::sync_channel::<Request>(4);
    std::thread::Builder::new()
      .name("video worker commands".into())
      .spawn(move || {
        while let Ok(request) = commands.recv() {
          if let Err(error) = protocol::write(&mut pipe, &request) {
            if errors.try_send(Err(error)).is_err() {
              eprintln!("Worker command error receiver closed");
            }
            break;
          }
        }
      })
      .map_err(|e| EngineError::io("command pipe writer", e))?;
    std::thread::Builder::new()
      .name("video worker events".into())
      .spawn(move || {
        let mut reader = BufReader::new(output);
        loop {
          match protocol::read::<Response>(&mut reader) {
            Ok(Some(response)) => {
              if send.send(Ok(response)).is_err() {
                break;
              }
            },
            Ok(None) => {
              if send
                .send(Err(EngineError::protocol("Worker event pipe closed")))
                .is_err()
              {
                eprintln!("Worker event receiver closed");
              }
              break;
            },
            Err(e) => {
              if send.send(Err(e)).is_err() {
                eprintln!("Worker protocol error receiver closed");
              }
              break;
            },
          }
        }
      })
      .map_err(|e| EngineError::io("worker reader", e))?;
    Ok(Self {
      child,
      input: Some(input),
      events,
      diagnostics,
      diagnostics_lost,
      _job: job,
    })
  }
  pub fn send(&mut self, request: &Request) -> Result<()> {
    self
      .input
      .as_mut()
      .ok_or_else(|| EngineError::protocol("Worker input closed"))?
      .try_send(request.clone())
      .map_err(|e| EngineError::new(ErrorKind::Saturation, "supervisor commands", e.to_string()))
  }
  pub fn receive(&self, timeout: Duration) -> Result<Option<Response>> {
    match self.events.recv_timeout(timeout) {
      Ok(v) => v.map(Some),
      Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
      Err(e) => Err(EngineError::protocol(e.to_string())),
    }
  }
  pub fn wait(&mut self, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    self.input.take();
    loop {
      if let Some(status) = self
        .child
        .try_wait()
        .map_err(|e| EngineError::io("worker exit", e))?
      {
        return if status.success() {
          Ok(())
        } else {
          Err(EngineError::new(
            ErrorKind::Native,
            "worker exit",
            status.to_string(),
          ))
        };
      }
      if start.elapsed() >= timeout {
        return Err(EngineError::new(
          ErrorKind::Timeout,
          "worker exit",
          "Worker did not exit before deadline",
        ));
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }
  pub fn terminate(&mut self) -> Result<()> {
    match self
      .child
      .try_wait()
      .map_err(|e| EngineError::io("worker status", e))?
    {
      Some(_) => Ok(()),
      None => self
        .child
        .kill()
        .map_err(|e| EngineError::io("owned worker terminate", e)),
    }
  }
  pub fn pid(&self) -> u32 {
    self.child.id()
  }
  pub fn diagnostic(&self) -> Option<String> {
    self.diagnostics.try_recv().ok()
  }
  pub fn diagnostics_lost(&self) -> u64 {
    self.diagnostics_lost.load(Ordering::Relaxed)
  }
}
impl Drop for WorkerProcess {
  fn drop(&mut self) {
    if let Err(e) = self.terminate() {
      eprintln!("Worker cleanup: {e}");
    }
  }
}
fn attach(child: &Child) -> Result<OwnedHandle> {
  // SAFETY: new unnamed non-inheritable job; handle ownership transfers once to OwnedHandle.
  let handle =
    unsafe { CreateJobObjectW(None, None) }.map_err(|e| EngineError::native("worker job", e))?;
  // SAFETY: CreateJobObjectW returned this new unique owned handle.
  let job = unsafe { OwnedHandle::from_raw_handle(handle.0) };
  let mut limit = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
  limit.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  // SAFETY: exact ABI size, and only this newly spawned child is assigned.
  unsafe {
    SetInformationJobObject(
      handle,
      JobObjectExtendedLimitInformation,
      &limit as *const _ as *const _,
      std::mem::size_of_val(&limit) as u32,
    )
    .map_err(|e| EngineError::native("worker job limits", e))?;
    AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle()))
      .map_err(|e| EngineError::native("worker job assign", e))?;
  }
  Ok(job)
}

/// Reassembles worker stderr into whole lines: pipe reads split a message anywhere (seen:
/// "Spanned area: " and "94720 pixels ..." arrived as two diagnostics). A line longer
/// than the bound is emitted in bounded pieces, so memory stays fixed.
#[derive(Default)]
struct DiagnosticLines {
  pending: Vec<u8>,
}
impl DiagnosticLines {
  const MAX_LINE: usize = 4096;
  fn push(&mut self, bytes: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    for byte in bytes {
      if *byte == b'\n' {
        lines.extend(self.take());
      } else {
        self.pending.push(*byte);
        if self.pending.len() >= Self::MAX_LINE {
          lines.extend(self.take());
        }
      }
    }
    lines
  }
  /// The final unterminated line, if any.
  fn finish(&mut self) -> Option<String> {
    self.take()
  }
  fn take(&mut self) -> Option<String> {
    let line = std::mem::take(&mut self.pending);
    let text = String::from_utf8_lossy(&line);
    let text = text.trim_end_matches('\r');
    (!text.trim().is_empty()).then(|| text.to_owned())
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  fn powershell() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("SystemRoot").expect("Windows root"))
      .join("System32/WindowsPowerShell/v1.0/powershell.exe")
  }
  #[test]
  fn worker_diagnostics_are_whole_lines_however_the_pipe_splits_them() {
    let mut lines = DiagnosticLines::default();
    assert!(lines.push(b"Spanned area: ").is_empty());
    assert_eq!(
      lines.push(b"94720 pixels lie on no display\r\nnext"),
      ["Spanned area: 94720 pixels lie on no display"]
    );
    assert_eq!(lines.push(b" line\n\n\r\nthird\n"), ["next line", "third"]);
    assert_eq!(lines.push(b"unterminated"), Vec::<String>::new());
    assert_eq!(lines.finish().as_deref(), Some("unterminated"));
    assert_eq!(lines.finish(), None);
    // An endless line is emitted in bounded pieces.
    let long = vec![b'x'; DiagnosticLines::MAX_LINE * 2 + 10];
    let pieces = lines.push(&long);
    assert_eq!(pieces.len(), 2);
    assert!(
      pieces
        .iter()
        .all(|piece| piece.len() == DiagnosticLines::MAX_LINE)
    );
    assert_eq!(lines.finish().map(|rest| rest.len()), Some(10));
  }
  #[test]
  fn excessive_worker_stderr_never_blocks_controls_and_its_losses_are_counted() {
    let script = "[Console]::Error.Write(('x' * 1048576)); [Console]::Out.WriteLine('{\"version\":1,\"session\":\"\",\"event\":{\"event\":\"hello\"}}'); Start-Sleep -Seconds 20";
    let mut worker = WorkerProcess::spawn_arguments(
      &powershell(),
      &["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .expect("owned noisy fixture");
    // A blocked stderr pipe never delivers hello, so the bound only covers a slow PowerShell
    // start under coverage on a shared runner; the child stays alive for 20 s.
    let hello = worker
      .receive(Duration::from_secs(15))
      .expect("bounded event")
      .expect("hello after stderr pressure");
    assert!(matches!(hello.event, crate::protocol::Event::Hello));
    assert!(worker.diagnostics_lost() > 0);
    assert!(worker.diagnostic().is_some());
    worker.terminate().expect("only owned child");
    assert!(worker.wait(Duration::from_secs(2)).is_err());
  }
  #[test]
  fn owned_job_and_protocol_have_bounded_failure_and_cleanup() {
    let script = "[Console]::Out.WriteLine('{\"version\":1,\"session\":\"\",\"event\":{\"event\":\"hello\"}}'); Start-Sleep -Seconds 20";
    let mut worker = WorkerProcess::spawn_arguments(
      &powershell(),
      &["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .expect("owned fixture child");
    let hello = worker
      .receive(Duration::from_secs(5))
      .expect("bounded fixture event")
      .expect("hello");
    assert!(matches!(hello.event, crate::protocol::Event::Hello));
    assert!(worker.wait(Duration::from_millis(10)).is_err());
    worker.terminate().expect("terminate owned fixture");
    assert!(worker.wait(Duration::from_secs(2)).is_err()); // A killed fixture is never a successful media result.
    let mut invalid = WorkerProcess::spawn_arguments(
      &powershell(),
      &[
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::Out.WriteLine('not-json')",
      ],
    )
    .expect("invalid fixture");
    assert!(invalid.receive(Duration::from_secs(5)).is_err());
    invalid.wait(Duration::from_secs(2)).expect("fixture exit");
    assert!(
      WorkerProcess::spawn(std::path::Path::new("Z:/missing-flinch-video-worker.exe")).is_err()
    );
  }
}
