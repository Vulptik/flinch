#![allow(clippy::expect_used)] // Test fixtures only; production remains deny.
use super::*;
use std::path::Path;

fn file_stream(path: &Path) -> IMFByteStream {
  let wide: Vec<u16> = path
    .as_os_str()
    .to_string_lossy()
    .encode_utf16()
    .chain([0])
    .collect();
  // SAFETY: terminated path; the test owns the new file.
  unsafe {
    MFCreateFile(
      MF_ACCESSMODE_WRITE,
      MF_OPENMODE_FAIL_IF_EXIST,
      MF_FILEFLAGS_NONE,
      windows::core::PCWSTR(wide.as_ptr()),
    )
  }
  .expect("file stream")
}

/// Every `tfra` entry must name a `moof` at its exact file position.
fn random_access_points_at_fragments(path: &Path) -> usize {
  let bytes = std::fs::read(path).expect("file");
  let mfra = bytes.windows(4).rposition(|w| w == b"tfra").expect("tfra") - 4;
  let version = bytes[mfra + 8];
  assert_eq!(version, 1, "the sink writes 64-bit entries");
  let lengths = u32::from_be_bytes(bytes[mfra + 16..mfra + 20].try_into().expect("lengths"));
  let entries = u32::from_be_bytes(bytes[mfra + 20..mfra + 24].try_into().expect("count"));
  let tail = ((lengths >> 4 & 3) + 1 + (lengths >> 2 & 3) + 1 + (lengths & 3) + 1) as usize;
  let mut at = mfra + 24;
  for _ in 0..entries {
    let offset = u64::from_be_bytes(bytes[at + 8..at + 16].try_into().expect("offset")) as usize;
    assert_eq!(&bytes[offset + 4..offset + 8], b"moof", "entry at {offset}");
    at += 16 + tail;
  }
  entries as usize
}

#[test]
fn small_chunks_force_alignment_and_every_offset_follows_the_padding() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  // Unaligned control: the plain file stream with 1 KiB "reads" would straddle constantly.
  let plain = directory.path().join("plain.mp4");
  super::super::mux::write_fixture_over(
    file_stream(&plain),
    super::super::fixtures::media_sized(3, 512, 64),
    512,
    64,
  );
  let chunk = 1024;
  let straddling = interop::count_straddles_in(&plain, chunk).expect("count");
  assert!(straddling > 3, "{straddling}");

  let aligned = directory.path().join("aligned.mp4");
  super::super::mux::write_fixture_over(
    AlignedStream::wrap(file_stream(&aligned), chunk),
    super::super::fixtures::media_sized(3, 512, 64),
    512,
    64,
  );
  assert_eq!(
    interop::count_straddles_in(&aligned, chunk).expect("count"),
    0
  );
  let grown = std::fs::metadata(&aligned).expect("aligned").len()
    - std::fs::metadata(&plain).expect("plain").len();
  assert!(grown >= 8 * straddling, "{grown} bytes of padding");
  // Patched base offsets: Windows decodes every numbered frame from the padded layout.
  let decoded = super::super::verify::verify(&aligned, true).expect("decode");
  assert_eq!(decoded.video_frames, 180);
  assert_eq!(decoded.numbered_frames_checked, 180);
  assert_eq!(decoded.structure.fragments, 10);
  // Patched random access and the patched-back mvhd: structure and duration intact.
  assert_eq!(random_access_points_at_fragments(&aligned), 1);
  assert_eq!(decoded.structure.duration_100ns, 30_000_000);
}

/// Top-level boxes as (type, bytes). `moov` holds creation/modification times, so two files
/// written moments apart can differ there while being laid out identically.
fn top_level(path: &Path) -> Vec<([u8; 4], Vec<u8>)> {
  let bytes = std::fs::read(path).expect("file");
  let mut boxes = Vec::new();
  let mut at = 0;
  while at + 8 <= bytes.len() {
    let size = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("size")) as usize;
    let kind: [u8; 4] = bytes[at + 4..at + 8].try_into().expect("kind");
    boxes.push((kind, bytes[at..at + size].to_vec()));
    at += size;
  }
  assert_eq!(at, bytes.len(), "whole boxes");
  boxes
}

#[test]
fn production_chunks_leave_small_files_unpadded_and_their_media_identical() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  let plain = directory.path().join("plain.mp4");
  let aligned = directory.path().join("aligned.mp4");
  super::super::mux::write_fixture_over(
    file_stream(&plain),
    super::super::fixtures::media_sized(2, 512, 64),
    512,
    64,
  );
  super::super::mux::write_fixture_over(
    AlignedStream::wrap(file_stream(&aligned), interop::READ_CHUNK),
    super::super::fixtures::media_sized(2, 512, 64),
    512,
    64,
  );
  let (plain, aligned) = (top_level(&plain), top_level(&aligned));
  // Same layout, no padding inserted, and every fragment and index byte-identical. (An earlier
  // whole-file comparison failed intermittently on the timestamps inside moov.)
  assert_eq!(
    plain
      .iter()
      .map(|(kind, bytes)| (*kind, bytes.len()))
      .collect::<Vec<_>>(),
    aligned
      .iter()
      .map(|(kind, bytes)| (*kind, bytes.len()))
      .collect::<Vec<_>>()
  );
  assert!(!aligned.iter().any(|(kind, _)| kind == b"free"));
  for ((kind, a), (_, b)) in plain.iter().zip(&aligned) {
    if kind != b"moov" {
      assert_eq!(a, b, "{}", String::from_utf8_lossy(kind));
    }
  }
}

#[test]
fn positions_translate_both_ways_around_inserted_padding() {
  let state = State {
    pads: vec![(1000, 24), (5000, 100)],
    ..Default::default()
  };
  assert_eq!(state.padding(), 124);
  for (sink, file) in [
    (0, 0),
    (999, 999),
    (1000, 1024),
    (4999, 5023),
    (5000, 5124),
    (9000, 9124),
  ] {
    assert_eq!(state.real(sink), file, "sink {sink}");
    assert_eq!(state.virtual_of(file), sink, "file {file}");
  }
  assert_eq!(
    whole_box(&[0, 0, 0, 8, b'm', b'o', b'o', b'f']),
    Some(*b"moof")
  );
  assert_eq!(whole_box(&[0, 0, 0, 9, b'm', b'o', b'o', b'f']), None);
  assert_eq!(whole_box(&[0, 0, 0]), None);
}

#[test]
fn the_stream_contract_translates_positions_and_refuses_what_it_does_not_support() {
  let _com = super::super::Apartment::new().expect("COM");
  let _mf = super::super::MediaRuntime::new().expect("MF");
  let directory = tempfile::tempdir().expect("directory");
  let path = directory.path().join("contract.mp4");
  let stream = AlignedStream::wrap(file_stream(&path), 64);
  // SAFETY: calls through the public COM interface on a live test object.
  unsafe {
    assert_ne!(
      stream.GetCapabilities().expect("caps") & MFBYTESTREAM_IS_WRITABLE,
      0
    );
    // An ordinary box passes through; a moof that would straddle 64 is padded to start on it.
    stream
      .Write(&[
        0, 0, 0, 40, b'f', b't', b'y', b'p', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
      ])
      .expect("ftyp");
    let mut moof = vec![0u8; 16];
    moof[..4].copy_from_slice(&16u32.to_be_bytes());
    moof[4..8].copy_from_slice(b"moof");
    moof[8..12].copy_from_slice(&8u32.to_be_bytes());
    moof[12..16].copy_from_slice(b"mfhd");
    assert_eq!(stream.Write(&moof).expect("moof"), 16);
    // The sink sees its own positions; the file holds 24 bytes of padding.
    assert_eq!(stream.GetCurrentPosition().expect("position"), 56);
    assert_eq!(stream.GetLength().expect("length"), 56);
    assert_eq!(std::fs::metadata(&path).expect("file").len(), 80);
    assert!(
      stream.IsEndOfStream().expect("eos").as_bool(),
      "positioned at the end"
    );
    assert_eq!(stream.Seek(msoBegin, 40, 0).expect("seek"), 40);
    assert_eq!(stream.Seek(msoCurrent, 4, 0).expect("relative"), 44);
    assert!(
      stream.Seek(msoCurrent, -100, 0).is_err(),
      "before the start"
    );
    assert!(
      stream.Seek(MFBYTESTREAM_SEEK_ORIGIN(7), 0, 0).is_err(),
      "unknown origin"
    );
    stream.SetCurrentPosition(56).expect("end");
    stream.SetLength(56).expect("length");
    assert_eq!(std::fs::metadata(&path).expect("file").len(), 80);
    let mut read = [0u8; 4];
    let mut count = 0;
    assert!(stream.Read(&mut read, &mut count).is_err(), "write-only");
    assert!(
      stream
        .BeginRead(&mut read, None::<&IMFAsyncCallback>, None)
        .is_err()
    );
    let foreign = MFCreateAsyncResult(None, None::<&IMFAsyncCallback>, None).expect("result");
    assert!(stream.EndRead(&foreign).is_err());
    assert!(
      stream.EndWrite(&foreign).is_err(),
      "a result this stream never issued"
    );
    stream.Flush().expect("flush");
    stream.Close().expect("close");
  }
  let bytes = std::fs::read(&path).expect("file");
  assert_eq!(&bytes[44..48], b"free");
  assert_eq!(&bytes[68..72], b"moof");
}
