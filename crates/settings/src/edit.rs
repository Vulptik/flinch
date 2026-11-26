//! User-originated preference changes. Refreshing a snapshot never emits one.

/// Built-in templates, shared with the capture engines so the reset controls
/// know when there is nothing to reset.
pub const DEFAULT_FILENAME_TEMPLATE: &str = "%DATETIME%_%APP%_%TYPE%";
pub const DEFAULT_SUBFOLDER_TEMPLATE: &str = "%YEAR%-%MONTH%";

/// A token that would exceed the template budget. The field says so rather than ignoring the click.
pub const TEMPLATE_LIMIT_ERROR: &str = "Template is too long";

/// A save folder the user can replace. Each save location mode owns its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderTarget {
  Together,
  Screenshots,
  Videos,
}

impl FolderTarget {
  pub const ALL: [Self; 3] = [Self::Together, Self::Screenshots, Self::Videos];
  pub fn pick_action(self) -> &'static str {
    match self {
      Self::Together => "pick-folder-together",
      Self::Screenshots => "pick-folder-screenshots",
      Self::Videos => "pick-folder-videos",
    }
  }
  pub fn reset_action(self) -> &'static str {
    match self {
      Self::Together => "reset-folder-together",
      Self::Screenshots => "reset-folder-screenshots",
      Self::Videos => "reset-folder-videos",
    }
  }
  pub fn from_pick_action(action: &str) -> Option<Self> {
    Self::ALL
      .into_iter()
      .find(|target| target.pick_action() == action)
  }
  pub fn from_reset_action(action: &str) -> Option<Self> {
    Self::ALL
      .into_iter()
      .find(|target| target.reset_action() == action)
  }
  fn slot(self) -> usize {
    match self {
      Self::Together => 27,
      Self::Screenshots => 28,
      Self::Videos => 29,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
  ScreenshotArea,
  ScreenshotFullscreen,
  ScreenshotWindow,
  ScreenshotEdit,
  CopyText,
  PickColor,
  LastRegion,
  Gif,
  VideoArea,
  VideoFullscreen,
  VideoWindow,
}

impl ShortcutAction {
  pub const ALL: [Self; 11] = [
    Self::ScreenshotArea,
    Self::ScreenshotFullscreen,
    Self::ScreenshotWindow,
    Self::ScreenshotEdit,
    Self::CopyText,
    Self::PickColor,
    Self::LastRegion,
    Self::Gif,
    Self::VideoArea,
    Self::VideoFullscreen,
    Self::VideoWindow,
  ];
  pub fn from_id(id: &str) -> Option<Self> {
    Self::ALL.into_iter().find(|action| action.id() == id)
  }
  pub fn id(self) -> &'static str {
    match self {
      Self::ScreenshotArea => "screenshot-area",
      Self::ScreenshotFullscreen => "screenshot-fullscreen",
      Self::ScreenshotWindow => "screenshot-window",
      Self::ScreenshotEdit => "screenshot-edit",
      Self::CopyText => "copy-text",
      Self::PickColor => "pick-color",
      Self::LastRegion => "last-region",
      Self::Gif => "gif",
      Self::VideoArea => "video-area",
      Self::VideoFullscreen => "video-fullscreen",
      Self::VideoWindow => "video-window",
    }
  }
  fn slot(self) -> usize {
    match self {
      Self::ScreenshotArea => 18,
      Self::ScreenshotFullscreen => 19,
      Self::ScreenshotWindow => 20,
      Self::ScreenshotEdit => 30,
      Self::CopyText => 32,
      Self::PickColor => 34,
      Self::LastRegion => 37,
      Self::Gif => 21,
      Self::VideoArea => 22,
      Self::VideoFullscreen => 23,
      Self::VideoWindow => 24,
    }
  }
}

/// A chosen microphone: its endpoint id and the name shown while it is unplugged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneChoice {
  pub id: String,
  pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreferenceEdit {
  Language(String),
  AutoStart(bool),
  CheckForUpdates(bool),
  NotificationsEnabled(bool),
  NotificationSound(bool),
  NotificationDuration(u64),
  NotificationPosition(String),
  ScreenshotFormat(String),
  ScreenshotQuality(u8),
  SelectionMagnifier(bool),
  CopyTextDirect(bool),
  CaptureDelay(u32),
  LargePngAsJpg(bool),
  GifFps(u32),
  GifDuration(u32),
  VideoFps(u32),
  VideoQuality(String),
  HardwareEncoding(bool),
  RecordAudio(bool),
  ShowCursor(bool),
  GifShowCursor(bool),
  ShowClicks(bool),
  GifShowClicks(bool),
  RecordMicrophone(bool),
  /// `None` follows the Windows default microphone.
  Microphone(Option<MicrophoneChoice>),
  SaveLocation(String),
  /// `None` restores the mode's built-in folder.
  SaveFolder(FolderTarget, Option<String>),
  Filename(String),
  Subfolder(String),
  ResetFilename,
  ResetSubfolder,
  Shortcut(ShortcutAction, String),
}

impl PreferenceEdit {
  /// Stable bounded slot shared by draft coalescing and inline UI save errors.
  pub fn slot(&self) -> usize {
    match self {
      Self::Language(_) => 0,
      Self::AutoStart(_) => 1,
      Self::CheckForUpdates(_) => 25,
      Self::NotificationsEnabled(_) => 2,
      Self::NotificationSound(_) => 3,
      Self::NotificationDuration(_) => 4,
      Self::NotificationPosition(_) => 5,
      Self::ScreenshotFormat(_) => 6,
      Self::ScreenshotQuality(_) => 7,
      Self::SelectionMagnifier(_) => 31,
      Self::CopyTextDirect(_) => 33,
      Self::CaptureDelay(_) => 36,
      Self::LargePngAsJpg(_) => 35,
      Self::GifFps(_) => 8,
      Self::GifDuration(_) => 9,
      Self::VideoFps(_) => 10,
      Self::VideoQuality(_) => 11,
      Self::HardwareEncoding(_) => 12,
      Self::RecordAudio(_) => 13,
      Self::ShowCursor(_) => 14,
      Self::GifShowCursor(_) => 26,
      Self::ShowClicks(_) => 38,
      Self::GifShowClicks(_) => 39,
      Self::RecordMicrophone(_) => 40,
      Self::Microphone(_) => 41,
      Self::SaveLocation(_) => 15,
      Self::SaveFolder(target, _) => target.slot(),
      Self::Filename(_) | Self::ResetFilename => 16,
      Self::Subfolder(_) | Self::ResetSubfolder => 17,
      Self::Shortcut(action, _) => action.slot(),
    }
  }
}

/// One latest value per ordinary preference. The fixed number of slots bounds
/// queued edits independently of typing speed or the persistence worker.
#[derive(Debug, Clone)]
pub struct EditDraft {
  edits: [Option<PreferenceEdit>; SLOT_COUNT],
}

/// How many preference slots exist: the draft, the inline error list and every presenter that
/// writes one share this count. Every `slot()` is below it.
pub const SLOT_COUNT: usize = 42;

impl Default for EditDraft {
  fn default() -> Self {
    Self {
      edits: std::array::from_fn(|_| None),
    }
  }
}

impl EditDraft {
  pub fn remove(&mut self, edit: &PreferenceEdit) {
    self.edits[edit.slot()] = None;
  }
  pub fn insert(&mut self, edit: PreferenceEdit) {
    let slot = edit.slot();
    self.edits[slot] = Some(edit);
  }

  pub fn is_empty(&self) -> bool {
    self.edits.iter().all(Option::is_none)
  }

  pub fn edits(&self) -> impl Iterator<Item = &PreferenceEdit> {
    self.edits.iter().flatten()
  }

  /// Acknowledging an older write cannot clear a newer edit to the same field.
  pub(crate) fn acknowledge(&mut self, saved: &Self) {
    for (pending, saved) in self.edits.iter_mut().zip(&saved.edits) {
      if pending == saved {
        *pending = None;
      }
    }
  }
}

/// Resolve a selection without accepting negative, stale or out-of-range UI
/// indices. Kept independent of Slint so this boundary is directly testable.
pub fn selection<T: Clone>(options: &[T], index: i32) -> Result<T, String> {
  usize::try_from(index)
    .ok()
    .and_then(|index| options.get(index))
    .cloned()
    .ok_or_else(|| "Native settings selection is out of range".to_owned())
}

/// Insert a supported ASCII token at the UTF-8 selection, preserving surrounding text.
pub fn template_tokens(text: &str, subfolder: bool) -> Vec<(usize, usize)> {
  const TOKENS: &[&str] = &[
    "%TYPE%",
    "%APP%",
    "%DATETIME%",
    "%TIMESTAMP%",
    "%DATE%",
    "%TIME%",
    "%YEAR%",
    "%MONTH%",
    "%DAY%",
    "%HOUR%",
    "%MINUTE%",
    "%SECOND%",
  ];
  let tokens: &[&str] = if subfolder {
    &["%YEAR%", "%MONTH%", "%DAY%"]
  } else {
    TOKENS
  };
  let mut spans = Vec::new();
  let mut offset = 0;
  while offset < text.len() {
    if let Some(token) = tokens
      .iter()
      .find(|token| text[offset..].starts_with(**token))
    {
      spans.push((offset, offset + token.len()));
      offset += token.len();
    } else if text[offset..].starts_with('%')
      && let Some(close) = text[offset + 1..].find('%')
    {
      offset += close + 2;
    } else if let Some(ch) = text[offset..].chars().next() {
      offset += ch.len_utf8();
    } else {
      break;
    }
  }
  spans
}

pub fn snap_template_cursor(text: &str, cursor: i32, previous: i32, subfolder: bool) -> i32 {
  for (start, end) in template_tokens(text, subfolder) {
    if cursor > start as i32 && cursor < end as i32 {
      return if cursor < previous {
        start as i32
      } else {
        end as i32
      };
    }
  }
  cursor
}

/// Apply one native text edit, expanding partial token deletions and rejecting insertion inside a token.
pub fn atomic_template_edit(
  previous: &str,
  candidate: &str,
  cursor: i32,
  subfolder: bool,
) -> (String, i32) {
  let prefix = previous
    .chars()
    .zip(candidate.chars())
    .take_while(|(a, b)| a == b)
    .map(|(ch, _)| ch.len_utf8())
    .sum::<usize>();
  let suffix = previous[prefix..]
    .chars()
    .rev()
    .zip(candidate[prefix..].chars().rev())
    .take_while(|(a, b)| a == b)
    .map(|(ch, _)| ch.len_utf8())
    .sum::<usize>();
  let old_end = previous.len() - suffix;
  let inserted = &candidate[prefix..candidate.len() - suffix];
  let mut start = prefix;
  let mut end = old_end;
  for (a, b) in template_tokens(previous, subfolder) {
    if prefix == old_end && prefix > a && prefix < b {
      return (previous.to_owned(), b as i32);
    }
    if prefix < b && old_end > a {
      start = start.min(a);
      end = end.max(b);
    }
  }
  let result = format!("{}{}{}", &previous[..start], inserted, &previous[end..]);
  let requested_cursor = if start != prefix || end != old_end {
    (start + inserted.len()) as i32
  } else {
    cursor
  };
  let (mut bounded, mut bounded_cursor) = limit_template_edit(previous, &result, requested_cursor);
  if result.len() > 100 {
    // A pasted tag must either fit whole or remain absent, like token-button insertion.
    let cutoff = bounded_cursor.max(0) as usize;
    if let Some((tag_start, _)) = template_tokens(&result, subfolder)
      .into_iter()
      .find(|(a, b)| *a < cutoff && cutoff < *b)
    {
      bounded.replace_range(tag_start..cutoff, "");
      bounded_cursor = tag_start as i32;
    }
  }
  (bounded, bounded_cursor)
}

pub fn insert_token(
  text: &str,
  token: &str,
  anchor: i32,
  cursor: i32,
  subfolder: bool,
) -> Result<(String, i32), String> {
  if template_tokens(token, subfolder) != [(0, token.len())] {
    return Err("Unknown template token".into());
  }
  let anchor = usize::try_from(anchor).map_err(|_| "Invalid template selection")?;
  let cursor = usize::try_from(cursor).map_err(|_| "Invalid template selection")?;
  let (mut start, mut end) = (anchor.min(cursor), anchor.max(cursor));
  if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
    return Err("Invalid UTF-8 template selection".into());
  }
  for (a, b) in template_tokens(text, subfolder) {
    if start == end && start > a && start < b {
      start = b;
      end = b;
    } else if start < b && end > a {
      start = start.min(a);
      end = end.max(b);
    }
  }
  let mut result = text.to_owned();
  result.replace_range(start..end, token);
  if result.len() > 100 {
    return Err(TEMPLATE_LIMIT_ERROR.into());
  }
  let cursor = i32::try_from(start + token.len()).map_err(|_| "Template is too long")?;
  Ok((result, cursor))
}

/// Bound user edits to the backend's UTF-8 byte budget, preserving text after the selection.
pub fn limit_template_edit(previous: &str, candidate: &str, cursor: i32) -> (String, i32) {
  if candidate.len() <= 100 {
    return (candidate.to_owned(), cursor);
  }
  if previous.len() > 100 {
    let mut end = 100;
    while !candidate.is_char_boundary(end) {
      end -= 1;
    }
    return (candidate[..end].to_owned(), cursor.clamp(0, end as i32));
  }
  let prefix = previous
    .chars()
    .zip(candidate.chars())
    .take_while(|(a, b)| a == b)
    .map(|(ch, _)| ch.len_utf8())
    .sum::<usize>();
  let suffix = previous[prefix..]
    .chars()
    .rev()
    .zip(candidate[prefix..].chars().rev())
    .take_while(|(a, b)| a == b)
    .map(|(ch, _)| ch.len_utf8())
    .sum::<usize>();
  let available = 100usize.saturating_sub(prefix + suffix);
  let insertion = &candidate[prefix..candidate.len() - suffix];
  let mut kept = available.min(insertion.len());
  while !insertion.is_char_boundary(kept) {
    kept -= 1;
  }
  let result = format!(
    "{}{}{}",
    &candidate[..prefix],
    &insertion[..kept],
    &candidate[candidate.len() - suffix..]
  );
  (result, (prefix + kept) as i32)
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn folder_targets_route_their_actions_and_keep_separate_slots() {
    let mut slots = std::collections::BTreeSet::new();
    for target in FolderTarget::ALL {
      assert_eq!(
        FolderTarget::from_pick_action(target.pick_action()),
        Some(target)
      );
      assert_eq!(
        FolderTarget::from_reset_action(target.reset_action()),
        Some(target)
      );
      assert!(slots.insert(PreferenceEdit::SaveFolder(target, None).slot()));
    }
    assert_eq!(FolderTarget::from_pick_action("open-videos"), None);
    assert_eq!(FolderTarget::from_reset_action("reset-filename"), None);
    let mut draft = EditDraft::default();
    for target in FolderTarget::ALL {
      draft.insert(PreferenceEdit::SaveFolder(target, None));
    }
    assert_eq!(draft.edits().count(), 3);
  }

  #[test]
  fn length_limit_never_splits_pasted_tokens() {
    for (token, subfolder) in [("%DATETIME%", false), ("%YEAR%", true)] {
      let previous = "a".repeat(96);
      let candidate = format!("{previous}{token}");
      assert_eq!(
        atomic_template_edit(&previous, &candidate, candidate.len() as i32, subfolder),
        (previous.clone(), 96)
      );
      let previous = "a".repeat(94) + "tail";
      let candidate = "a".repeat(94) + token + "tail";
      assert_eq!(
        atomic_template_edit(&previous, &candidate, (94 + token.len()) as i32, subfolder),
        (previous, 94)
      );
    }
  }
  #[test]
  fn subfolder_tokens_match_its_renderer() {
    assert_eq!(
      template_tokens("%YEAR%-%MONTH%-%DATETIME%-%APP%", true),
      [(0, 6), (7, 14)]
    );
    assert_eq!(template_tokens("%DAY%", true), [(0, 5)]);
    assert_eq!(snap_template_cursor("%APP%", 2, 0, true), 2);
    assert_eq!(
      atomic_template_edit("%APP%", "%AxPP%", 3, true),
      ("%AxPP%".into(), 3)
    );
    assert_eq!(
      atomic_template_edit("%YEAR%", "%YEAR", 5, true),
      ("".into(), 0)
    );
    for token in [
      "%APP%",
      "%TYPE%",
      "%DATETIME%",
      "%TIMESTAMP%",
      "%DATE%",
      "%TIME%",
      "%HOUR%",
      "%MINUTE%",
      "%SECOND%",
    ] {
      assert!(template_tokens(token, true).is_empty());
      assert!(insert_token("", token, 0, 0, true).is_err());
      assert_eq!(template_tokens(token, false), [(0, token.len())]);
    }
  }
  #[test]
  fn tags_are_atomic_for_deletion_replacement_and_insertion() {
    assert!(template_tokens("%whatever%", false).is_empty());
    assert!(template_tokens("%whatever%APP%", false).is_empty());
    assert_eq!(
      atomic_template_edit("%whatever%", "%whateve%", 8, false),
      ("%whateve%".into(), 8)
    );
    assert_eq!(
      atomic_template_edit("é%APP%_x", "é%APP_x", 6, false),
      ("é_x".into(), 2)
    );
    assert_eq!(
      atomic_template_edit("%APP%_%TYPE%", "APP%_%TYPE%", 0, false),
      ("_%TYPE%".into(), 0)
    );
    assert_eq!(
      atomic_template_edit("%APP%", "%AxPP%", 3, false),
      ("%APP%".into(), 5)
    );
    assert_eq!(
      atomic_template_edit("a%APP%b", "a%X%b", 3, false),
      ("aXb".into(), 2)
    );
    assert_eq!(snap_template_cursor("%APP%", 4, 5, false), 0);
    assert_eq!(snap_template_cursor("%APP%", 1, 0, false), 5);
    assert_eq!(
      atomic_template_edit("%APP%_", "%APP%", 5, false),
      ("%APP%".into(), 5)
    );
  }
  #[test]
  fn template_edits_bound_typing_paste_and_unicode_without_losing_suffix() {
    let full = "a".repeat(100);
    assert_eq!(
      limit_template_edit(&full, &(full.clone() + "b"), 101),
      (full.clone(), 100)
    );
    let previous = "a".repeat(95) + "end";
    let candidate = "a".repeat(95) + "éééend";
    assert_eq!(
      limit_template_edit(&previous, &candidate, 101),
      ("a".repeat(95) + "éend", 97)
    );
    assert_eq!(
      insert_token(&full, "%APP%", 100, 100, false),
      Err(TEMPLATE_LIMIT_ERROR.to_owned())
    );
    assert_eq!(
      limit_template_edit("abc", "abXYZc", 5),
      ("abXYZc".into(), 5)
    );
  }
  #[test]
  fn token_insertion_replaces_selection_and_rejects_split_utf8() {
    assert_eq!(
      insert_token("aébc", "%APP%", 5, 3, false),
      Ok(("aé%APP%".into(), 8))
    );
    assert_eq!(
      insert_token("abc", "%YEAR%", 1, 1, false),
      Ok(("a%YEAR%bc".into(), 7))
    );
    assert!(insert_token("é", "%APP%", 1, 1, false).is_err());
    assert!(insert_token("abc", "%UNKNOWN%", 0, 0, false).is_err());
  }

  #[test]
  fn selection_rejects_stale_indices_without_choosing_a_default() {
    assert_eq!(selection(&[30, 60], 1), Ok(60));
    assert!(selection(&[30, 60], -1).is_err());
    assert!(selection(&[30, 60], 2).is_err());
    assert!(selection::<u32>(&[], 0).is_err());
  }

  #[test]
  fn coalescing_preserves_unrelated_preferences_and_newer_edits_after_acknowledgment() {
    use PreferenceEdit::*;
    let mut edits = vec![
      Language("en".into()),
      AutoStart(false),
      CheckForUpdates(true),
      NotificationsEnabled(true),
      NotificationSound(false),
      NotificationDuration(3000),
      NotificationPosition("bottom-right".into()),
      ScreenshotFormat("png".into()),
      ScreenshotQuality(90),
      GifFps(15),
      GifDuration(30),
      VideoFps(60),
      VideoQuality("high".into()),
      HardwareEncoding(true),
      RecordAudio(false),
      ShowCursor(true),
      GifShowCursor(false),
      ShowClicks(true),
      GifShowClicks(false),
      RecordMicrophone(true),
      Microphone(None),
      SelectionMagnifier(true),
      CopyTextDirect(true),
      CaptureDelay(5),
      LargePngAsJpg(false),
      SaveLocation("single_folder".into()),
      Filename("capture_%TYPE%".into()),
      Subfolder("%YEAR%".into()),
    ];
    edits.extend(ShortcutAction::ALL.map(|action| Shortcut(action, "Ctrl+F9".into())));
    let mut draft = EditDraft::default();
    for edit in &edits {
      draft.insert(edit.clone());
    }
    assert_eq!(draft.edits().count(), edits.len());
    assert!(
      edits
        .iter()
        .all(|edit| draft.edits().any(|pending| pending == edit))
    );
    let saved = draft.clone();
    draft.insert(Language("fr".into()));
    draft.insert(NotificationSound(true));
    draft.acknowledge(&saved);
    let retained = draft.edits().cloned().collect::<Vec<_>>();
    assert_eq!(
      retained,
      vec![Language("fr".into()), NotificationSound(true)]
    );
    let saved = draft.clone();
    draft.acknowledge(&saved);
    assert!(draft.is_empty());
  }

  #[test]
  fn all_shortcut_fields_coalesce_independently_and_round_trip_ids() {
    let mut draft = EditDraft::default();
    for action in ShortcutAction::ALL {
      assert_eq!(ShortcutAction::from_id(action.id()), Some(action));
      draft.insert(PreferenceEdit::Shortcut(action, "Ctrl+F9".into()));
      draft.insert(PreferenceEdit::Shortcut(action, "Ctrl+F10".into()));
    }
    draft.insert(PreferenceEdit::RecordAudio(true));
    assert_eq!(draft.edits().count(), ShortcutAction::ALL.len() + 1);
    assert!(ShortcutAction::from_id("unknown").is_none());
  }
}
