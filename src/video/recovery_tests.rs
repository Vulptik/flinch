use super::*;

/// A later launch: markers in these tests were written by the test process itself.
const OTHER: u32 = 0;

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
  let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
  out.extend(kind);
  out.extend(body);
  out
}
/// One initialized fragment holding a single 60 FPS sample: the smallest recoverable recording.
fn fragment() -> Vec<u8> {
  let mut tkhd = vec![0; 16];
  tkhd[12..].copy_from_slice(&1u32.to_be_bytes());
  let mut mdhd = vec![0; 16];
  mdhd[12..].copy_from_slice(&60u32.to_be_bytes());
  let mut track = atom(b"tkhd", &tkhd);
  track.extend(atom(b"mdia", &atom(b"mdhd", &mdhd)));
  let mut tfhd = vec![0, 0, 0, 0x18];
  tfhd.extend(1u32.to_be_bytes());
  tfhd.extend(1u32.to_be_bytes());
  tfhd.extend(4u32.to_be_bytes());
  let mut traf = atom(b"tfhd", &tfhd);
  traf.extend(atom(b"tfdt", &[0; 8]));
  let mut trun = vec![0; 4];
  trun.extend(1u32.to_be_bytes());
  traf.extend(atom(b"trun", &trun));
  let mut out = atom(b"ftyp", b"isom0000");
  out.extend(atom(b"moov", &atom(b"trak", &track)));
  out.extend(atom(b"moof", &atom(b"traf", &traf)));
  out.extend(atom(b"mdat", &[1, 2, 3, 4]));
  out
}

struct Layout {
  _dir: tempfile::TempDir,
  root: PathBuf,
  folder: PathBuf,
  engine_output: PathBuf,
  destination: PathBuf,
}
fn layout(name: &str, gif: bool) -> Layout {
  let dir = tempfile::tempdir().expect("temp");
  let root = dir.path().join("recordings");
  let folder = root.join("video-clip-1-2");
  let engine_output = folder.join("clip.flinch-part.mp4");
  let destination = dir.path().join("Videos").join(name);
  write_marker(&engine_output, &destination, gif).expect("marker");
  Layout {
    _dir: dir,
    root,
    folder,
    engine_output,
    destination,
  }
}
/// A session the worker left mid-recording: a complete fragment and a torn one after it.
fn interrupted(layout: &Layout) -> PathBuf {
  let session = layout.folder.join(format!("{SESSION_DIRECTORY_PREFIX}7-8"));
  std::fs::create_dir_all(&session).expect("session");
  let partial = session.join(PARTIAL);
  let mut bytes = fragment();
  bytes.extend([0, 0, 0, 100, b'm', b'o', b'o', b'f']);
  std::fs::write(&partial, bytes).expect("partial");
  partial
}
fn no_gif(_: &Path, _: &Path) -> Result<(), String> {
  Err("not a GIF session".into())
}
fn marker(layout: &Layout) -> Marker {
  serde_json::from_slice(&std::fs::read(layout.folder.join(MARKER)).expect("marker"))
    .expect("parse marker")
}

#[test]
fn an_interrupted_recording_is_saved_to_its_destination_without_the_torn_tail() {
  let layout = layout("clip.mp4", false);
  interrupted(&layout);
  let results = recover_all(&layout.root, OTHER, &no_gif);
  assert_eq!(
    results,
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
  let saved = std::fs::read(&layout.destination).expect("saved");
  assert_eq!(saved.len(), fragment().len());
  assert!(!layout.folder.exists(), "temporary folder removed");
  let leftovers: Vec<_> = std::fs::read_dir(layout.destination.parent().expect("folder"))
    .expect("videos")
    .flatten()
    .map(|entry| entry.file_name())
    .collect();
  assert_eq!(leftovers, ["clip.mp4"], "no staged file left behind");
}

#[test]
fn a_taken_destination_keeps_its_file_and_the_recovery_takes_the_next_free_name() {
  let layout = layout("clip.mp4", false);
  interrupted(&layout);
  std::fs::create_dir_all(layout.destination.parent().expect("folder")).expect("videos");
  std::fs::write(&layout.destination, b"a newer recording").expect("existing");
  let next = layout.destination.with_file_name("clip (1).mp4");
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(layout.folder.clone(), Ok(Recovered::Saved(next.clone())))]
  );
  assert_eq!(
    std::fs::read(&layout.destination).expect("existing"),
    b"a newer recording"
  );
  assert!(next.is_file());
}

#[test]
fn a_file_the_engine_published_but_the_app_never_moved_is_preferred() {
  let layout = layout("clip.mp4", false);
  // Only the published file is recoverable, so a save proves it was the source.
  std::fs::write(interrupted(&layout), b"stale partial").expect("partial");
  std::fs::write(&layout.engine_output, fragment()).expect("published");
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
  assert_eq!(
    std::fs::read(&layout.destination).expect("saved").len(),
    fragment().len()
  );
}

#[test]
fn a_recording_still_held_open_is_left_for_the_next_launch() {
  let layout = layout("clip.mp4", false);
  let partial = interrupted(&layout);
  let held = std::fs::File::open(&partial).expect("worker handle");
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(layout.folder.clone(), Ok(Recovered::InUse))]
  );
  assert!(partial.is_file() && !layout.destination.exists());
  assert!(marker(&layout).failed.is_none(), "not a failure");
  drop(held);
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
}

#[test]
fn a_session_that_recorded_nothing_is_removed() {
  let layout = layout("clip.mp4", false);
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(layout.folder.clone(), Ok(Recovered::Empty))]
  );
  assert!(!layout.folder.exists() && !layout.destination.exists());
}

#[test]
fn an_unrecoverable_recording_is_kept_marked_and_never_retried() {
  let layout = layout("clip.mp4", false);
  let partial = interrupted(&layout);
  std::fs::write(&partial, b"not an mp4").expect("garbage");
  let results = recover_all(&layout.root, OTHER, &no_gif);
  assert_eq!(results.len(), 1);
  let reason = results[0].1.as_ref().expect_err("unrecoverable");
  assert!(reason.contains("Recover"), "{reason}");
  assert!(partial.is_file(), "the media is kept");
  assert!(!layout.destination.exists());
  let videos: Vec<_> = std::fs::read_dir(layout.destination.parent().expect("folder"))
    .expect("videos")
    .collect();
  assert!(videos.is_empty(), "the staged file is removed");
  assert_eq!(marker(&layout).failed.as_deref(), Some(reason.as_str()));
  assert!(
    recover_all(&layout.root, OTHER, &no_gif).is_empty(),
    "no retry"
  );
}

#[test]
fn an_interrupted_gif_is_recovered_then_encoded_to_its_destination() {
  let layout = layout("clip.gif", true);
  interrupted(&layout);
  let encoded = std::sync::Mutex::new(Vec::new());
  let encode = |input: &Path, output: &Path| {
    encoded
      .lock()
      .expect("calls")
      .push((input.to_path_buf(), output.to_path_buf()));
    assert_eq!(
      std::fs::read(input).expect("intermediate").len(),
      fragment().len()
    );
    std::fs::write(output, b"GIF89a").map_err(|e| e.to_string())
  };
  assert_eq!(
    recover_all(&layout.root, OTHER, &encode),
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
  assert_eq!(encoded.lock().expect("calls")[0].1, layout.destination);
  assert!(!layout.folder.exists());

  // An encoding failure keeps the session and records why.
  let layout = self::layout("clip.gif", true);
  interrupted(&layout);
  let results = recover_all(&layout.root, OTHER, &|_: &Path, _: &Path| {
    Err("palette".into())
  });
  assert_eq!(results[0].1, Err("palette".into()));
  assert!(layout.folder.is_dir() && !layout.destination.exists());
  assert_eq!(marker(&layout).failed.as_deref(), Some("palette"));
}

#[test]
fn folders_without_a_marker_and_a_missing_root_are_left_alone() {
  let dir = tempfile::tempdir().expect("temp");
  let legacy = dir.path().join("video-old-1-2");
  std::fs::create_dir_all(legacy.join(format!("{SESSION_DIRECTORY_PREFIX}1-2"))).expect("legacy");
  assert!(recover_all(dir.path(), OTHER, &no_gif).is_empty());
  assert!(legacy.is_dir());
  assert!(recover_all(&dir.path().join("missing"), OTHER, &no_gif).is_empty());
}

#[test]
fn a_damaged_marker_is_reported_and_its_folder_kept() {
  let layout = layout("clip.mp4", false);
  std::fs::write(layout.folder.join(MARKER), b"{").expect("damaged");
  let results = recover_all(&layout.root, OTHER, &no_gif);
  assert!(
    results[0].1.as_ref().is_err_and(|e| e.contains("Parse")),
    "{results:?}"
  );
  assert!(layout.folder.is_dir());
}

#[test]
fn the_running_process_never_recovers_its_own_sessions() {
  // A recording started during launch has a marker before its worker opens the partial.
  let layout = layout("clip.mp4", false);
  assert!(recover_all(&layout.root, std::process::id(), &no_gif).is_empty());
  assert!(layout.folder.is_dir());
  assert_eq!(marker(&layout).process, std::process::id());
}

#[test]
fn a_staged_file_from_an_attempt_cut_short_is_replaced() {
  let layout = layout("clip.mp4", false);
  interrupted(&layout);
  let videos = layout.destination.parent().expect("folder");
  std::fs::create_dir_all(videos).expect("videos");
  let stale = videos.join(".flinch-recovering-video-clip-1-2.mp4");
  std::fs::write(&stale, b"half written").expect("stale stage");
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
  assert!(!stale.exists());
}

#[test]
fn a_saved_recording_whose_folder_cannot_be_removed_is_never_saved_twice() {
  use std::os::windows::fs::OpenOptionsExt;
  let layout = layout("clip.mp4", false);
  interrupted(&layout);
  // Something else (an antivirus scan, a dying worker) still holds a file in the folder.
  let held = std::fs::OpenOptions::new()
    .create(true)
    .truncate(true)
    .write(true)
    .share_mode(0)
    .open(layout.folder.join("held.log"))
    .expect("held file");
  assert_eq!(
    recover_all(&layout.root, OTHER, &no_gif),
    [(
      layout.folder.clone(),
      Ok(Recovered::Saved(layout.destination.clone()))
    )]
  );
  assert!(layout.destination.is_file() && layout.folder.is_dir());
  let note = marker(&layout).failed.expect("marked");
  assert!(note.starts_with("Recovered to"), "{note}");
  drop(held);
  assert!(
    recover_all(&layout.root, OTHER, &no_gif).is_empty(),
    "no second copy"
  );
}
