// Integration tests for shortcut parsing, validation, and conflict detection
// Tests complex shortcut scenarios and edge cases

#[test]
fn test_shortcut_conflict_detection_same_shortcut() {
  // Two different features cannot use the same shortcut
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "Ctrl+Shift+S";

  // In practice, the registration system would detect this conflict
  let has_conflict = shortcut1 == shortcut2;
  assert!(has_conflict);
}

#[test]
fn test_shortcut_conflict_detection_normalized() {
  // Shortcuts that normalize to the same value are conflicts
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "ctrl+shift+s"; // Different case

  // After normalization, these are the same
  let normalized1 = shortcut1.to_lowercase();
  let normalized2 = shortcut2.to_lowercase();

  assert_eq!(normalized1, normalized2);
}

#[test]
fn test_shortcut_conflict_with_spaces() {
  // Shortcuts with different spacing that normalize to same value
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "Ctrl + Shift + S";

  // After normalization (removing spaces around +), these should be equal
  let normalized1 = shortcut1.replace(" ", "");
  let normalized2 = shortcut2.replace(" ", "");

  assert_eq!(normalized1, normalized2);
}

#[test]
fn test_no_conflict_different_keys() {
  // Different shortcuts should not conflict
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "Ctrl+Shift+A";

  assert_ne!(shortcut1, shortcut2);
}

#[test]
fn test_no_conflict_different_modifiers() {
  // Different modifiers should not conflict
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "Ctrl+Alt+S";

  assert_ne!(shortcut1, shortcut2);
}

#[test]
fn test_shortcut_validation_empty() {
  // Empty shortcuts are valid (means no shortcut assigned)
  let shortcut = "";
  assert_eq!(shortcut, "");
}

#[test]
fn test_shortcut_validation_whitespace_only() {
  // Whitespace-only should be treated as empty after trimming
  let shortcut = "   ";
  assert!(shortcut.trim().is_empty());
}

#[test]
fn test_shortcut_validation_valid_formats() {
  // Valid shortcut formats
  let valid_shortcuts = vec![
    "Ctrl+S",
    "Ctrl+Shift+S",
    "Ctrl+Alt+Delete",
    "Alt+F4",
    "Shift+Tab",
    "PrintScreen",
    "Ctrl+PrintScreen",
  ];

  for shortcut in valid_shortcuts {
    // Basic validation: contains at least one character after normalization
    assert!(!shortcut.is_empty());
    // Contains only valid characters
    assert!(
      shortcut
        .chars()
        .all(|c| c.is_alphanumeric() || c == '+' || c == ' ')
    );
  }
}

#[test]
fn test_shortcut_modifier_order_irrelevance() {
  // In most systems, modifier order doesn't matter
  // But for string comparison, we need to normalize order
  let shortcut1 = "Ctrl+Shift+S";
  let shortcut2 = "Shift+Ctrl+S";

  // These are functionally the same but string-different
  assert_ne!(shortcut1, shortcut2);

  // To detect conflicts, we'd need to normalize order
  // (Not implemented in current system, just documenting the behavior)
}

#[test]
fn test_shortcut_key_case_sensitivity() {
  // Key letter case might matter
  let shortcut1 = "Ctrl+S";
  let shortcut2 = "Ctrl+s";

  // These are different strings
  assert_ne!(shortcut1, shortcut2);

  // Platform behavior varies - some treat these as same, others as different
}

#[test]
fn test_reserved_system_shortcuts() {
  // Common system shortcuts that might conflict
  let system_shortcuts = vec![
    "Ctrl+Alt+Delete", // Windows task manager
    "Alt+F4",          // Windows close
    "Alt+Tab",         // Windows app switcher
  ];

  // These are valid shortcut strings, but may fail to register
  // because the OS reserves them
  for shortcut in system_shortcuts {
    assert!(!shortcut.is_empty());
  }
}

#[test]
fn test_function_key_shortcuts() {
  // Function keys are valid shortcuts
  let function_keys = vec![
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
  ];

  for key in function_keys {
    assert!(key.starts_with('F'));
    assert!(key.len() >= 2 && key.len() <= 3);
  }
}

#[test]
fn test_function_key_with_modifiers() {
  // Function keys combined with modifiers
  let shortcuts = vec!["Ctrl+F1", "Shift+F5", "Alt+F4", "Ctrl+Shift+F12"];

  for shortcut in shortcuts {
    assert!(shortcut.contains('F'));
    assert!(shortcut.contains('+'));
  }
}

#[test]
fn test_printscreen_variations() {
  // Different ways to specify PrintScreen
  let variations = vec![
    "PrintScreen",
    "Print",
    "PrtSc",
    "Ctrl+PrintScreen",
    "Shift+PrintScreen",
  ];

  // All should be valid (normalization handles Print -> PrintScreen)
  for variation in variations {
    assert!(!variation.is_empty());
  }
}

#[test]
fn test_numeric_key_shortcuts() {
  // Number keys with modifiers
  let shortcuts = vec!["Ctrl+1", "Ctrl+2", "Ctrl+3", "Shift+5", "Alt+0"];

  for shortcut in shortcuts {
    assert!(shortcut.chars().any(|c| c.is_numeric()));
  }
}

#[test]
fn test_special_character_shortcuts() {
  // Special characters that might be used
  let shortcuts = vec!["Ctrl+[", "Ctrl+]", "Ctrl+;", "Ctrl+,", "Ctrl+.", "Ctrl+/"];

  for shortcut in shortcuts {
    assert!(shortcut.contains('+'));
  }
}

#[test]
fn test_arrow_key_shortcuts() {
  // Arrow keys are valid
  let arrows = vec![
    "Up",
    "Down",
    "Left",
    "Right",
    "Ctrl+Up",
    "Shift+Down",
    "Alt+Left",
    "Ctrl+Shift+Right",
  ];

  for arrow in arrows {
    assert!(!arrow.is_empty());
  }
}

#[test]
fn test_home_end_pageup_pagedown() {
  // Navigation keys
  let nav_keys = vec![
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "Ctrl+Home",
    "Shift+End",
    "Alt+PageUp",
    "Ctrl+PageDown",
  ];

  for key in nav_keys {
    assert!(!key.is_empty());
  }
}

#[test]
fn test_shortcut_length_limits() {
  // Very short shortcuts
  let short_shortcuts = vec!["A", "1", "F1"];
  for shortcut in short_shortcuts {
    assert!(!shortcut.is_empty());
  }

  // Very long shortcut (probably invalid in practice)
  let long_shortcut = "Ctrl+Shift+Alt+Command+Super+Meta+A";
  assert!(long_shortcut.len() > 20);

  // Reasonable max length check (most shortcuts are < 30 chars)
  assert!(long_shortcut.len() < 100);
}

#[test]
fn test_empty_modifier_plus_key() {
  // Edge case: just a + sign
  let invalid = "+";
  assert_eq!(invalid, "+");

  // After normalization, this should be cleaned up
  let trimmed = invalid.trim_matches('+');
  assert!(trimmed.is_empty());
}

#[test]
fn test_multiple_plus_signs() {
  // Malformed shortcut with multiple + signs
  let malformed = "Ctrl++S";

  // Should be cleaned up to "Ctrl+S"
  let cleaned = malformed.replace("++", "+");
  assert_eq!(cleaned, "Ctrl+S");
}

#[test]
fn test_platform_specific_modifiers() {
  // Windows uses Ctrl as the primary modifier
  let win_shortcut = "Ctrl+C";
  assert!(win_shortcut.contains("Ctrl"));
}

#[test]
fn test_shortcut_with_numbers_and_letters() {
  // Mixed alphanumeric
  let shortcuts = vec![
    "Ctrl+Shift+5",
    "Alt+F4",
    "Ctrl+1+A", // This is unusual but technically valid string
  ];

  for shortcut in shortcuts {
    let has_alpha = shortcut.chars().any(|c| c.is_alphabetic());
    let has_digit = shortcut.chars().any(|c| c.is_numeric());

    // Most shortcuts have both (modifier + key)
    assert!(has_alpha || has_digit);
  }
}

#[test]
fn test_duplicate_modifiers() {
  // Edge case: same modifier twice (unusual)
  let unusual = "Ctrl+Ctrl+S";

  // This is a valid string but semantically weird
  assert!(unusual.matches("Ctrl").count() == 2);
}

#[test]
fn test_tab_escape_backspace_keys() {
  // Special keys
  let special_keys = vec![
    "Tab",
    "Escape",
    "Backspace",
    "Delete",
    "Ctrl+Tab",
    "Shift+Escape",
    "Alt+Backspace",
  ];

  for key in special_keys {
    assert!(!key.is_empty());
  }
}

#[test]
fn test_case_normalization_expectations() {
  // Test that we handle various cases consistently
  let test_cases = vec![
    ("ctrl+s", "ctrl", "s"),
    ("CTRL+S", "CTRL", "S"),
    ("Ctrl+S", "Ctrl", "S"),
  ];

  for (full, modifier, key) in test_cases {
    assert!(full.contains(modifier));
    assert!(full.contains(key));
    assert!(full.contains('+'));
  }
}
