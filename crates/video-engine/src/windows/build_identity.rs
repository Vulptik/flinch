//! Lab provenance: hash the worker binary after the session, never on the Prepare or media path.
use crate::{EngineError, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{
  io::Read,
  path::{Path, PathBuf},
};
use windows::Win32::{Foundation::NTSTATUS, Security::Cryptography::*};
const MAX_BINARY: u64 = 512 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildIdentity {
  pub engine_version: String,
  pub architecture: String,
  pub debug_assertions: bool,
  pub os_build: u32,
  pub executable: PathBuf,
  pub executable_bytes: u64,
  pub executable_sha256: String,
}
fn status(status: NTSTATUS, stage: &str) -> Result<()> {
  if status.0 < 0 {
    Err(EngineError {
      native_code: Some(status.0),
      ..EngineError::new(
        ErrorKind::Native,
        stage,
        format!("NTSTATUS 0x{:08x}", status.0 as u32),
      )
    })
  } else {
    Ok(())
  }
}
struct Hash(Option<BCRYPT_HASH_HANDLE>);
impl Hash {
  fn close(&mut self) -> Result<()> {
    if let Some(handle) = self.0.take() {
      // SAFETY: this hash's unique owning reference is released once, after all update/finalize calls.
      status(unsafe { BCryptDestroyHash(handle) }, "build hash release")?;
    }
    Ok(())
  }
}
impl Drop for Hash {
  fn drop(&mut self) {
    if let Err(error) = self.close() {
      eprintln!("Build identity cleanup: {error}");
    }
  }
}
fn digest(mut reader: impl Read, bytes: u64) -> Result<String> {
  if bytes > MAX_BINARY {
    return Err(EngineError::new(
      ErrorKind::Capability,
      "build identity",
      "Executable exceeds the 512 MiB hashing budget",
    ));
  }
  let mut handle = BCRYPT_HASH_HANDLE::default();
  // SAFETY: Windows 10 pseudo-handle selects SHA-256; CNG allocates its own hash storage.
  // https://learn.microsoft.com/en-us/windows/win32/seccng/cng-algorithm-pseudo-handles
  status(
    unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut handle, None, None, 0) },
    "build hash create",
  )?;
  let mut hash = Hash(Some(handle));
  let result = (|| {
    let mut storage = vec![0u8; 32768];
    let mut remaining = bytes;
    while remaining > 0 {
      let length = storage.len().min(remaining as usize);
      let count = reader
        .read(&mut storage[..length])
        .map_err(|error| EngineError::io("build hash read", error))?;
      if count == 0 {
        return Err(EngineError::protocol("Executable shrank while hashing"));
      }
      // SAFETY: hash is live and the immutable slice remains valid for this synchronous update.
      status(
        unsafe { BCryptHashData(handle, &storage[..count], 0) },
        "build hash update",
      )?;
      remaining -= count as u64;
    }
    if reader
      .read(&mut storage[..1])
      .map_err(|error| EngineError::io("build hash extent", error))?
      != 0
    {
      return Err(EngineError::protocol("Executable grew while hashing"));
    }
    let mut digest = [0u8; 32];
    // SAFETY: exactly 32 output bytes match SHA-256 and this hash has not been finalized before.
    status(
      unsafe { BCryptFinishHash(handle, &mut digest, 0) },
      "build hash finish",
    )?;
    Ok(
      digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>(),
    )
  })();
  let cleanup = hash.close();
  match (result, cleanup) {
    (Ok(value), Ok(())) => Ok(value),
    (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
    (Err(mut original), Err(cleanup)) => {
      original.reason.push_str(&format!("; cleanup: {cleanup}"));
      Err(original)
    },
  }
}
/// An executable opened before its worker starts. Sharing excludes writes and renames, so the bytes
/// hashed later are the bytes that process loaded.
pub struct HeldExecutable {
  path: PathBuf,
  file: std::fs::File,
}
impl HeldExecutable {
  pub fn open(path: &Path) -> Result<Self> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
      .read(true)
      .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
      .open(path)
      .map_err(|error| EngineError::io("build file", error))?;
    Ok(Self {
      path: path.to_owned(),
      file,
    })
  }
  pub fn identify(self) -> Result<BuildIdentity> {
    let bytes = self
      .file
      .metadata()
      .map_err(|error| EngineError::io("build metadata", error))?
      .len();
    Ok(BuildIdentity {
      engine_version: env!("CARGO_PKG_VERSION").into(),
      architecture: std::env::consts::ARCH.into(),
      debug_assertions: cfg!(debug_assertions),
      os_build: super::gpu::os_build()?,
      executable: self.path,
      executable_bytes: bytes,
      executable_sha256: digest(self.file, bytes)?,
    })
  }
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
  use super::*;
  #[test]
  fn executable_identity_is_stable_and_matches_the_sha256_reference_vector() {
    let root = tempfile::tempdir().expect("temp");
    let path = root.path().join("binary-fixture.exe");
    std::fs::write(&path, b"abc").expect("fixture");
    let held = HeldExecutable::open(&path).expect("held binary");
    // While held, the file can be read (a worker can load it) but neither rewritten nor replaced.
    assert_eq!(std::fs::read(&path).expect("shared read"), b"abc");
    assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
    assert!(std::fs::rename(&path, root.path().join("moved.exe")).is_err());
    assert!(std::fs::remove_file(&path).is_err());
    let identity = held.identify().expect("software-only binary identity");
    assert_eq!(identity.executable_bytes, 3);
    assert_eq!(
      identity.executable_sha256,
      "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(identity.architecture, "x86_64");
    assert_eq!(std::fs::read(&path).expect("unchanged fixture"), b"abc");
    std::fs::write(&path, b"replaced after hashing").expect("released after identify");
  }
  #[test]
  fn a_held_executable_still_starts_a_process_and_hashes_the_loaded_bytes() {
    let root = tempfile::tempdir().expect("temp");
    let system = std::env::var_os("SystemRoot").expect("Windows directory");
    let path = root.path().join("where-copy.exe");
    std::fs::copy(
      std::path::Path::new(&system)
        .join("System32")
        .join("where.exe"),
      &path,
    )
    .expect("console tool copy");
    let held = HeldExecutable::open(&path).expect("held before launch");
    // A windowless console tool, as the lab launches its worker after holding its own binary.
    let status = std::process::Command::new(&path)
      .arg("/?")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .expect("process image loads through the read-only share");
    assert!(status.success());
    let identity = held.identify().expect("hash after the process exits");
    assert_eq!(
      identity.executable_bytes,
      std::fs::metadata(&path).expect("size").len()
    );
    assert!(HeldExecutable::open(&root.path().join("missing.exe")).is_err());
  }
  #[test]
  fn hash_rejects_changed_sizes_failed_reads_and_oversized_inputs() {
    assert!(digest(&b"a"[..], 2).is_err());
    assert!(digest(&b"ab"[..], 1).is_err());
    assert!(digest(&b""[..], MAX_BINARY + 1).is_err());
    assert_eq!(
      digest(&b""[..], 0).expect("empty SHA256"),
      "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    struct Failed;
    impl Read for Failed {
      fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("scripted disk failure"))
      }
    }
    assert_eq!(
      digest(Failed, 1).expect_err("failed read").stage,
      "build hash read"
    );
    assert!(status(NTSTATUS(-1), "scripted CNG failure").is_err());
  }
}
