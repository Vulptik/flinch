#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::*;

#[test]
fn only_boundaries_after_the_moof_header_and_before_the_payload_straddle() {
  let b = 2 * READ_CHUNK;
  // Measured behaviour: a boundary 1..=7 bytes into the moof box header is harmless.
  assert!(!straddles(b - 4, b + 400));
  assert!(!straddles(b - 7, b + 400));
  assert!(straddles(b - 8, b + 400));
  assert!(straddles(b - 200, b + 1));
  // A boundary exactly at the payload start, or at the moof start, is harmless.
  assert!(!straddles(b - 376, b));
  assert!(!straddles(b, b + 376));
  assert!(!straddles(1000, 2000));
}

/// Test-only inverse of realignment: insert a `free` box of `pad` bytes before the
/// `moof_index`-th fragment, keeping every absolute offset valid.
fn insert_free(path: &Path, moof_index: usize, pad: u64) {
  let mut file = File::open(path).expect("open");
  let length = file.metadata().expect("size").len();
  let list = boxes(&mut file, length).expect("boxes");
  let mut out = Vec::new();
  let mut shift = 0;
  let mut moved = Vec::new();
  let mut seen = 0;
  for item in &list {
    let mut bytes = read_box_any(&mut file, item);
    if &item.kind == b"moof" {
      if seen == moof_index {
        let mut free = vec![0u8; pad as usize];
        free[..4].copy_from_slice(&(pad as u32).to_be_bytes());
        free[4..8].copy_from_slice(b"free");
        out.extend_from_slice(&free);
        shift += pad;
      }
      seen += 1;
      shift_moof(&mut bytes, shift).expect("moof");
      moved.push((item.start, item.start + shift));
    }
    if &item.kind == b"mfra" {
      shift_mfra(&mut bytes, &moved).expect("mfra");
    }
    out.extend_from_slice(&bytes);
  }
  drop(file);
  fs::write(path, out).expect("rewrite");
}
fn read_box_any(file: &mut File, item: &Entry) -> Vec<u8> {
  file.seek(SeekFrom::Start(item.start)).expect("seek");
  let mut bytes = vec![0u8; item.size as usize];
  file.read_exact(&mut bytes).expect("read");
  bytes
}
fn moofs(path: &Path) -> Vec<u64> {
  let mut file = File::open(path).expect("open");
  let length = file.metadata().expect("size").len();
  boxes(&mut file, length)
    .expect("boxes")
    .iter()
    .filter(|item| &item.kind == b"moof")
    .map(|item| item.start)
    .collect()
}

#[test]
fn a_straddling_fragment_truncates_windows_playback_until_the_file_is_realigned() {
  let _com = crate::windows::Apartment::new().expect("COM");
  let _mf = crate::windows::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  let path = directory.path().join("straddle.mp4");
  crate::windows::mux::write_fixture(
    &path,
    crate::windows::fixtures::media_sized(1, 512, 64),
    512,
    64,
  );
  let clean = crate::windows::verify::verify(&path, true).expect("clean fixture");
  assert_eq!(clean.video_frames, 60);
  assert_eq!(count_straddles(&path).expect("count"), 0);
  let untouched = realign(&path, || Ok(())).expect("no-op");
  assert_eq!(untouched.moved, 0);

  // Put the third fragment's header across the 512 KiB chunk boundary, 40 bytes in. (The
  // first two fragments are parsed when the file opens and are immune.)
  let starts = moofs(&path);
  assert!(starts.len() >= 3, "{starts:?}");
  let boundary = 2 * READ_CHUNK;
  insert_free(&path, 2, boundary - 40 - starts[2]);
  assert_eq!(moofs(&path)[2], boundary - 40);
  assert_eq!(count_straddles(&path).expect("count"), 1);
  // The structure is intact, yet Windows' MPEG-4 source ends the stream at the straddle.
  let structure = crate::recovery::inspect(&path).expect("inspect");
  assert_eq!(structure.chunk_straddles, 1);
  let error = crate::windows::verify::verify(&path, true).expect_err("truncated playback");
  assert!(
    error.to_string().contains("last decoded video time"),
    "{error}"
  );

  let bytes_before = fs::metadata(&path).expect("size").len();
  let mut checkpoints = 0;
  let report = realign(&path, || {
    checkpoints += 1;
    Ok(())
  })
  .expect("realign");
  assert_eq!(report.moved, 1);
  assert!(checkpoints > 0);
  assert_eq!(
    fs::metadata(&path).expect("size").len(),
    bytes_before + report.padding_bytes
  );
  assert_eq!(moofs(&path)[2], boundary);
  assert_eq!(count_straddles(&path).expect("count"), 0);
  let repaired = crate::windows::verify::verify(&path, true).expect("full playback");
  assert_eq!(repaired.video_frames, 60);
  assert_eq!(repaired.numbered_frames_checked, 60);
  assert!(!path.with_extension("realign.partial").exists());
}

#[test]
fn realignment_failures_leave_the_original_and_no_partial() {
  let _com = crate::windows::Apartment::new().expect("COM");
  let _mf = crate::windows::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  let path = directory.path().join("straddle.mp4");
  crate::windows::mux::write_fixture(
    &path,
    crate::windows::fixtures::media_sized(1, 512, 64),
    512,
    64,
  );
  let starts = moofs(&path);
  insert_free(&path, 2, 2 * READ_CHUNK - 40 - starts[2]);
  let original = fs::read(&path).expect("original");

  // Cancellation during the copy.
  let cancelled = realign(&path, || {
    Err(EngineError::new(ErrorKind::Cancelled, "test", "cancelled"))
  })
  .expect_err("cancelled");
  assert_eq!(cancelled.kind, ErrorKind::Cancelled);
  assert_eq!(fs::read(&path).expect("kept"), original);
  assert!(!path.with_extension("realign.partial").exists());

  // A leftover partial is never overwritten, and never deleted as if it were ours.
  fs::write(path.with_extension("realign.partial"), b"someone else's").expect("leftover");
  assert!(realign(&path, || Ok(())).is_err());
  assert_eq!(fs::read(&path).expect("kept"), original);
  assert_eq!(
    fs::read(path.with_extension("realign.partial")).expect("leftover kept"),
    b"someone else's"
  );
  fs::remove_file(path.with_extension("realign.partial")).expect("cleanup");

  // Truncated or malformed structure is refused, never "fixed".
  let truncated = directory.path().join("truncated.mp4");
  fs::write(&truncated, &original[..original.len() - 3]).expect("truncated");
  assert!(realign(&truncated, || Ok(())).is_err());
  let orphan = directory.path().join("orphan.mp4");
  let mut bytes = original.clone();
  let last_moof = *moofs(&path).last().expect("moof") as usize;
  bytes.truncate(
    last_moof
      + u32::from_be_bytes([
        bytes[last_moof],
        bytes[last_moof + 1],
        bytes[last_moof + 2],
        bytes[last_moof + 3],
      ]) as usize,
  );
  fs::write(&orphan, &bytes).expect("orphan");
  assert!(count_straddles(&orphan).is_err(), "moof without mdat");
}

#[test]
fn recovered_prefixes_are_realigned_for_windows_playback() {
  let _com = crate::windows::Apartment::new().expect("COM");
  let _mf = crate::windows::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  let path = directory.path().join("straddle.mp4");
  crate::windows::mux::write_fixture(
    &path,
    crate::windows::fixtures::media_sized(1, 512, 64),
    512,
    64,
  );
  let starts = moofs(&path);
  insert_free(&path, 2, 2 * READ_CHUNK - 40 - starts[2]);
  let recovered = directory.path().join("recovered.mp4");
  let report = crate::recovery::recover(&path, &recovered).expect("recover");
  assert_eq!(report.chunk_straddles, 1, "the input's risk is reported");
  assert_eq!(count_straddles(&recovered).expect("count"), 0);
  let decoded = crate::windows::verify::verify(&recovered, true).expect("full playback");
  assert_eq!(decoded.video_frames, 60);
}
