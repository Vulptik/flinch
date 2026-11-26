// src/filename/mod.rs
//
// Filename template engine for screenshots, recordings, and GIFs.
//
// Templates are strings that may contain tokens like %APP%, %DATE%, etc.
// Literal parts between tokens are validated against Windows filename rules.
// Tokens are expanded at capture time using FilenameContext.

use chrono::{DateTime, Local};

/// All supported template tokens, in the order shown to the user.
pub const TOKENS: &[(&str, &str)] = &[
  ("%TYPE%", "Capture type (screenshot, recording, gif)"),
  ("%APP%", "App name (e.g. chrome, Code)"),
  ("%DATETIME%", "Date and time (2026-04-07_14-30-00-123)"),
  ("%DATE%", "Date (2026-04-07)"),
  ("%TIME%", "Time (14-30-00)"),
  ("%YEAR%", "Year (2026)"),
  ("%MONTH%", "Month (04)"),
  ("%DAY%", "Day (07)"),
  ("%HOUR%", "Hour (14)"),
  ("%MINUTE%", "Minute (30)"),
  ("%SECOND%", "Second (00)"),
  ("%TIMESTAMP%", "Unix timestamp in seconds"),
];

/// The default template used when none is configured.
pub const DEFAULT_TEMPLATE: &str = flinch_settings::edit::DEFAULT_FILENAME_TEMPLATE;

/// Sample `%APP%` value for settings previews. Never the product's own name,
/// which would read as a fixed part of every filename.
pub const PREVIEW_APP: &str = "chrome";

/// Separators removed with an `%APP%` that has no value.
const APP_SEPARATORS: &[char] = &['_', '-', ' ', '.'];

/// Characters forbidden anywhere in a Windows filename.
const WINDOWS_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// Windows reserved filenames (checked case-insensitively against the stem).
const RESERVED_NAMES: &[&str] = &[
  "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
  "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Context passed to `render_template` at capture time.
pub struct FilenameContext<'a> {
  /// Capture type string: "screenshot", "recording", or "gif".
  pub capture_type: &'a str,
  /// Exe stem of the detected foreground app (e.g. "Code", "chrome"), or None.
  pub app_name: Option<&'a str>,
  /// Timestamp to use — caller should capture this at the moment of capture.
  pub now: DateTime<Local>,
}

/// Sanitize an app name so it is safe as part of a Windows filename.
fn sanitize_app_name(name: &str) -> String {
  let s: String = name
    .chars()
    .map(|c| {
      if WINDOWS_FORBIDDEN.contains(&c) || c == '\0' || c.is_control() {
        '_'
      } else {
        c
      }
    })
    .collect();
  let trimmed = s.trim();
  if trimmed.is_empty() {
    "unknown".to_string()
  } else {
    trimmed.to_string()
  }
}

/// Split recognized tags from literal text; unknown percent patterns remain literal.
fn template_segments(mut remaining: &str) -> impl Iterator<Item = (&str, bool)> {
  std::iter::from_fn(move || {
    if remaining.is_empty() {
      return None;
    }
    let end = match remaining.find('%') {
      Some(0) => remaining[1..]
        .find('%')
        .map_or(remaining.len(), |close| close + 2),
      Some(start) => start,
      None => remaining.len(),
    };
    let (segment, rest) = remaining.split_at(end);
    let is_token = TOKENS.iter().any(|(token, _)| *token == segment);
    remaining = rest;
    Some((segment, is_token))
  })
}
/// Validate a filename template.
///
/// Checks:
/// 1. Not empty / too long.
/// 2. No forbidden Windows chars in literal segments.
/// 3. Unknown percent-delimited text remains literal.
/// 4. The rendered output (with placeholders) is not a reserved Windows name
///    and doesn't end with dots. Trailing spaces are trimmed on output.
pub fn validate_template(template: &str) -> Result<(), String> {
  if template.trim().is_empty() {
    return Err("Template cannot be empty.".to_string());
  }
  if template.len() > 100 {
    return Err("Template is too long (max 100 characters).".to_string());
  }

  for ch in template.chars() {
    if ch <= '\u{1f}' {
      return Err("Control characters are not allowed in Windows filenames.".to_owned());
    }
    if WINDOWS_FORBIDDEN.contains(&ch) {
      return Err(format!(
        "Character '{}' is not allowed in Windows filenames.",
        ch
      ));
    }
  }
  // Render with safe placeholder values and check the result
  let preview = render_template(
    template,
    &FilenameContext {
      capture_type: "screenshot",
      app_name: Some("App"),
      now: chrono::Local::now(),
    },
  );

  if preview
    .trim_matches(|c: char| c == ' ' || c == '.')
    .is_empty()
  {
    return Err("Template produces an empty filename.".to_string());
  }
  if preview.ends_with('.') {
    return Err("Filename cannot end with a dot.".to_string());
  }

  // Check reserved Windows names against the stem (before any dot)
  let stem = preview
    .split('.')
    .next()
    .unwrap_or(&preview)
    .trim_matches(' ')
    .to_uppercase();
  if RESERVED_NAMES.contains(&stem.as_str()) {
    return Err(format!("\"{}\" is a reserved Windows filename.", preview));
  }

  Ok(())
}

/// Render a template to a filename stem (no extension).
///
/// Unknown tokens are literal text and are left as-is.
/// `%APP%` is sanitized to strip any characters forbidden in Windows filenames.
/// A capture without a source app, such as full screen, leaves `%APP%` out with
/// one neighboring separator. A stem left empty falls back to the capture type.
pub fn render_template(template: &str, ctx: &FilenameContext) -> String {
  let app = ctx
    .app_name
    .map(|name| sanitize_app_name(name).chars().take(50).collect::<String>());

  let mut rendered = String::with_capacity(template.len());
  let mut skip_separator = false;
  let mut app_omitted = false;
  for (segment, is_token) in template_segments(template) {
    let segment = if std::mem::take(&mut skip_separator) {
      segment.strip_prefix(APP_SEPARATORS).unwrap_or(segment)
    } else {
      segment
    };
    if !is_token {
      rendered.push_str(segment);
      continue;
    }
    match segment {
      "%APP%" => match &app {
        Some(app) => rendered.push_str(app),
        None => {
          app_omitted = true;
          if rendered.ends_with(APP_SEPARATORS) {
            rendered.pop();
          } else {
            skip_separator = true;
          }
        },
      },
      "%TYPE%" => rendered.push_str(ctx.capture_type),
      "%TIMESTAMP%" => rendered.push_str(&ctx.now.timestamp().to_string()),
      token => {
        let format = match token {
          "%DATETIME%" => "%Y-%m-%d_%H-%M-%S-%3f",
          "%DATE%" => "%Y-%m-%d",
          "%TIME%" => "%H-%M-%S",
          "%YEAR%" => "%Y",
          "%MONTH%" => "%m",
          "%DAY%" => "%d",
          "%HOUR%" => "%H",
          "%MINUTE%" => "%M",
          "%SECOND%" => "%S",
          _ => {
            rendered.push_str(token);
            continue;
          },
        };
        rendered.push_str(&ctx.now.format(format).to_string());
      },
    }
  }
  let rendered = rendered.trim_end_matches(' ');
  if app_omitted && rendered.trim_matches([' ', '.']).is_empty() {
    ctx.capture_type.to_owned()
  } else {
    rendered.to_owned()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn supported_tokens_match_settings_input_blocks() {
    for (token, _) in TOKENS {
      assert_eq!(
        flinch_settings::edit::template_tokens(token, false),
        [(0, token.len())]
      );
    }
  }
  #[test]
  fn spaces_remain_editable_and_only_trailing_output_spaces_are_trimmed() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("App"),
      now: chrono::Local::now(),
    };
    for template in ["%APP% ", " %APP% %TYPE% ", "my capture folder "] {
      assert!(validate_template(template).is_ok());
    }
    assert_eq!(render_template(" %APP%  %TYPE% ", &ctx), " App  screenshot");
    assert!(validate_template("CON ").is_err());
    assert!(validate_template(" CON ").is_err());
    assert!(validate_template("name. ").is_err());
  }
  #[test]
  fn valid_leading_dots_are_allowed_but_empty_names_and_controls_are_not() {
    assert!(validate_template(".capture").is_ok());
    assert!(validate_template("..").is_err());
    assert!(validate_template("name.").is_err());
    assert!(validate_template("name\n").is_err());
    assert!(validate_template("name\0").is_err());
  }

  #[test]
  fn render_uses_validated_token_boundaries() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("flinch"),
      now: chrono::Local::now(),
    };
    let date = ctx.now.format("%Y-%m-%d");
    let time = ctx.now.format("%H-%M-%S");
    for (template, expected) in [
      (
        "%DATE%APP%TIME%_%APP%_%TYPE%",
        format!("{date}APP{time}_flinch_screenshot"),
      ),
      ("%DATE%%APP%%TIME%", format!("{date}flinch{time}")),
      ("é_%APP%_%APP%_100%", "é_flinch_flinch_100%".to_string()),
    ] {
      assert!(validate_template(template).is_ok(), "{template}");
      assert_eq!(render_template(template, &ctx), expected);
    }
  }

  #[test]
  fn render_does_not_expand_tokens_inside_replacements_or_unknown_tokens() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("%TYPE%"),
      now: chrono::Local::now(),
    };
    assert_eq!(render_template("%APP%_%TYPE%", &ctx), "%TYPE%_screenshot");
    assert!(validate_template("%UNKNOWN%APP%").is_ok());
    assert_eq!(render_template("%UNKNOWN%APP%", &ctx), "%UNKNOWN%APP%");
  }

  #[test]
  fn test_render_basic() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("Code"),
      now: chrono::Local::now(),
    };
    let result = render_template("%TYPE%_%APP%", &ctx);
    assert_eq!(result, "screenshot_Code");
  }

  #[test]
  fn test_render_no_app() {
    let ctx = FilenameContext {
      capture_type: "gif",
      app_name: None,
      now: chrono::Local::now(),
    };
    let result = render_template("%TYPE%_%APP%", &ctx);
    assert_eq!(result, "gif");
  }

  #[test]
  fn a_capture_without_an_app_leaves_app_out_with_one_separator() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: None,
      now: chrono::Local::now(),
    };
    let date = ctx.now.format("%Y-%m-%d");
    for (template, expected) in [
      ("%DATE%_%APP%_%TYPE%", format!("{date}_screenshot")),
      ("%APP%_%TYPE%", "screenshot".to_string()),
      ("%APP%-%DATE%", date.to_string()),
      ("%TYPE% %APP%", "screenshot".to_string()),
      ("%DATE%%APP%%TYPE%", format!("{date}screenshot")),
      ("my %APP% capture", "my capture".to_string()),
      ("%APP%", "screenshot".to_string()),
      ("%APP%_", "screenshot".to_string()),
    ] {
      assert_eq!(render_template(template, &ctx), expected, "{template}");
    }
    assert!(!render_template(DEFAULT_TEMPLATE, &ctx).contains("unknown"));
  }

  #[test]
  fn test_render_sanitizes_app_name() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("my:app/name"),
      now: chrono::Local::now(),
    };
    let result = render_template("%APP%", &ctx);
    assert_eq!(result, "my_app_name");
  }

  #[test]
  fn test_validate_ok() {
    assert!(validate_template("%TYPE%_%DATETIME%").is_ok());
    assert!(validate_template("capture_%APP%_%DATE%").is_ok());
    assert!(validate_template("flinch_%TIMESTAMP%").is_ok());
  }

  #[test]
  fn test_validate_empty() {
    assert!(validate_template("").is_err());
    assert!(validate_template("   ").is_err());
  }

  #[test]
  fn test_validate_forbidden_chars() {
    assert!(validate_template("file:name").is_err());
    assert!(validate_template("file*name").is_err());
    assert!(validate_template("file?name").is_err());
    assert!(validate_template("file<name").is_err());
    assert!(validate_template("file>name").is_err());
    assert!(validate_template("file|name").is_err());
  }

  #[test]
  fn test_validate_unknown_token() {
    assert!(validate_template("%UNKNOWN%").is_ok());
    assert!(validate_template("%type%").is_ok());
    assert!(validate_template("%what:ever%").is_err());
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some("App"),
      now: chrono::Local::now(),
    };
    assert_eq!(render_template("%whatever%_%APP%", &ctx), "%whatever%_App");
  }

  #[test]
  fn test_validate_lone_percent() {
    // A lone '%' with no closing '%' is treated as a literal character (valid on Windows)
    assert!(validate_template("file%name").is_ok());
  }

  #[test]
  fn test_validate_too_long() {
    let long = "a".repeat(101);
    assert!(validate_template(&long).is_err());
    let ok = "a".repeat(100);
    assert!(validate_template(&ok).is_ok());
  }

  #[test]
  fn test_render_truncates_long_app_name() {
    let ctx = FilenameContext {
      capture_type: "screenshot",
      app_name: Some(&"x".repeat(100)),
      now: chrono::Local::now(),
    };
    let result = render_template("%APP%", &ctx);
    assert_eq!(result.len(), 50);
  }
}
