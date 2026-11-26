//! The Microphone picker's entries: Windows' default, then each connected microphone, and the
//! chosen one kept (marked not connected) while it is unplugged, so a choice is never lost.

/// One picker entry. `id` is `None` for "Windows Default".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
  pub id: Option<String>,
  /// Saved with the choice, so an unplugged microphone keeps its name.
  pub name: String,
  pub label: String,
}

/// The entries and the selected index.
/// `available` are the connected microphones (id, name) in Windows' order; `chosen` the saved
/// choice (id, name). `unplugged` holds `{name}`.
pub fn picker(
  default_label: &str,
  unplugged: &str,
  available: &[(String, String)],
  chosen: Option<(&str, Option<&str>)>,
) -> (Vec<Entry>, usize) {
  let mut entries = Vec::with_capacity(available.len() + 2);
  entries.push(Entry {
    id: None,
    name: String::new(),
    label: default_label.to_owned(),
  });
  for (id, name) in available {
    entries.push(Entry {
      id: Some(id.clone()),
      name: name.clone(),
      label: name.clone(),
    });
  }
  let Some((id, name)) = chosen else {
    return (entries, 0);
  };
  if let Some(index) = entries
    .iter()
    .position(|entry| entry.id.as_deref() == Some(id))
  {
    return (entries, index);
  }
  let name = name.filter(|name| !name.trim().is_empty()).unwrap_or(id);
  entries.push(Entry {
    id: Some(id.to_owned()),
    name: name.to_owned(),
    label: unplugged.replace("{name}", name),
  });
  let index = entries.len() - 1;
  (entries, index)
}

/// A level meter's bar for a peak (0 to 1): a decibel scale from -60 dB, so speech fills most
/// of it and silence none; it falls back gradually instead of flickering.
pub fn bar(previous: f32, peak: f32) -> f32 {
  let target = if peak > 0.001 {
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
  } else {
    0.0
  };
  if target >= previous {
    target
  } else {
    (previous - 0.06).max(target)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn connected() -> Vec<(String, String)> {
    vec![
      ("usb".into(), "Microphone (USB)".into()),
      ("headset".into(), "Headset".into()),
    ]
  }

  #[test]
  fn the_default_comes_first_and_a_connected_choice_is_selected() {
    let (entries, index) = picker(
      "Windows Default",
      "{name} (Not Connected)",
      &connected(),
      None,
    );
    let labels: Vec<_> = entries.iter().map(|entry| entry.label.as_str()).collect();
    assert_eq!(labels, ["Windows Default", "Microphone (USB)", "Headset"]);
    assert_eq!((index, entries[0].id.as_deref()), (0, None));
    let (_, index) = picker(
      "D",
      "{name} x",
      &connected(),
      Some(("headset", Some("Old name"))),
    );
    assert_eq!(index, 2);
  }

  #[test]
  fn an_unplugged_choice_stays_listed_and_selected_by_its_saved_name() {
    let (entries, index) = picker(
      "Windows Default",
      "{name} (Not Connected)",
      &connected(),
      Some(("gone", Some("Studio Mic"))),
    );
    assert_eq!(index, 3);
    assert_eq!(entries[3].label, "Studio Mic (Not Connected)");
    assert_eq!(
      (entries[3].id.as_deref(), entries[3].name.as_str()),
      (Some("gone"), "Studio Mic")
    );
    // Nothing connected and no saved name: the id stands in.
    let (entries, index) = picker("D", "{name}!", &[], Some(("gone", None)));
    assert_eq!((index, entries[1].label.as_str()), (1, "gone!"));
  }

  #[test]
  fn the_meter_rises_at_once_falls_gradually_and_rests_at_zero() {
    assert_eq!(bar(0.0, 0.0), 0.0);
    assert_eq!(bar(0.0, 1.0), 1.0);
    let speech = bar(0.0, 0.1);
    assert!((speech - 2.0 / 3.0).abs() < 0.001, "{speech}");
    let falling = bar(1.0, 0.0);
    assert!(falling > 0.9 && falling < 1.0);
    let mut level = 1.0;
    for _ in 0..20 {
      level = bar(level, 0.0);
    }
    assert_eq!(level, 0.0);
  }
}
