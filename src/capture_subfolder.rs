use chrono::{DateTime, Local};

/// Supported tokens for the single-folder subfolder template.
pub const TOKENS: &[&str] = &["%YEAR%", "%MONTH%", "%DAY%"];

/// Default subfolder template used when the user keeps the built-in behavior.
pub const DEFAULT_TEMPLATE: &str = flinch_settings::edit::DEFAULT_SUBFOLDER_TEMPLATE;

const WINDOWS_FORBIDDEN: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
const RESERVED_NAMES: &[&str] = &[
  "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
  "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn validate_template(template: &str) -> Result<(), String> {
  if template.trim().is_empty() {
    return Ok(());
  }
  if template.len() > 100 {
    return Err("Template is too long (max 100 characters).".to_string());
  }

  for ch in template.chars() {
    if ch <= '\u{1f}' {
      return Err("Control characters are not allowed in Windows folder names.".to_owned());
    }
    if WINDOWS_FORBIDDEN.contains(&ch) {
      return Err(format!(
        "Character '{}' is not allowed in Windows folder names.",
        ch
      ));
    }
  }
  let preview = render_template(
    template,
    &SubfolderContext {
      now: chrono::Local::now(),
    },
  );

  if preview.trim_matches([' ', '.']).is_empty() {
    return Err("Template produces an empty folder name.".to_string());
  }
  if preview.ends_with('.') {
    return Err("Folder name cannot end with a dot.".to_string());
  }

  let upper = preview
    .split('.')
    .next()
    .unwrap_or(&preview)
    .trim_matches(' ')
    .to_uppercase();
  if RESERVED_NAMES.contains(&upper.as_str()) {
    return Err(format!(
      "\"{}\" is a reserved Windows folder name.",
      preview
    ));
  }

  Ok(())
}

pub struct SubfolderContext {
  pub now: DateTime<Local>,
}

pub fn render_template(template: &str, ctx: &SubfolderContext) -> String {
  if template.trim().is_empty() {
    return String::new();
  }

  let mut rendered = String::with_capacity(template.len());
  let mut offset = 0;
  for (start, end) in flinch_settings::edit::template_tokens(template, true) {
    rendered.push_str(&template[offset..start]);
    let format = match &template[start..end] {
      "%YEAR%" => "%Y",
      "%MONTH%" => "%m",
      "%DAY%" => "%d",
      token => {
        rendered.push_str(token);
        offset = end;
        continue;
      },
    };
    rendered.push_str(&ctx.now.format(format).to_string());
    offset = end;
  }
  rendered.push_str(&template[offset..]);
  rendered.trim_end_matches(' ').to_owned()
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn leading_dots_are_valid_and_reserved_folder_stems_are_rejected() {
    assert!(validate_template(".captures").is_ok());
    assert!(validate_template("CON.txt").is_err());
    assert!(validate_template("..").is_err());
    assert!(validate_template("folder\nname").is_err());
    assert!(validate_template("folder\0name").is_err());
  }
  #[test]
  fn spaces_remain_editable_and_only_trailing_output_spaces_are_trimmed() {
    let ctx = SubfolderContext {
      now: chrono::Local::now(),
    };
    assert!(validate_template(" My  captures ").is_ok());
    assert!(validate_template("%YEAR% ").is_ok());
    assert_eq!(render_template(" My  captures ", &ctx), " My  captures");
    assert!(validate_template(" CON ").is_err());
    assert!(validate_template("name. ").is_err());
  }

  #[test]
  fn empty_template_is_allowed() {
    assert!(validate_template("").is_ok());
    assert!(validate_template("   ").is_ok());
  }

  #[test]
  fn valid_templates_render_expected_values() {
    let now = chrono::Local::now();
    let preview = render_template(DEFAULT_TEMPLATE, &SubfolderContext { now });
    assert!(preview.contains('-'));
  }

  #[test]
  fn invalid_characters_are_rejected() {
    assert!(validate_template("%YEAR%/%MONTH%").is_err());
    assert!(validate_template("%YEAR%\\%MONTH%").is_err());
  }

  #[test]
  fn percent_boundaries_match_input_blocks() {
    let ctx = SubfolderContext {
      now: chrono::Local::now(),
    };
    assert_eq!(render_template("%unknown%YEAR%", &ctx), "%unknown%YEAR%");
    assert_eq!(render_template("%%YEAR%", &ctx), "%%YEAR%");
    assert_eq!(
      render_template("%YEAR%MONTH%DAY%", &ctx),
      format!("{}MONTH{}", ctx.now.format("%Y"), ctx.now.format("%d"))
    );
    assert_eq!(
      render_template("%YEAR%%MONTH%%DAY%", &ctx),
      ctx.now.format("%Y%m%d").to_string()
    );
    for token in TOKENS {
      assert_eq!(
        flinch_settings::edit::template_tokens(token, true),
        [(0, token.len())]
      );
    }
  }
  #[test]
  fn unknown_tokens_remain_literal() {
    assert!(validate_template("%DATETIME%").is_ok());
    assert!(validate_template("%type%").is_ok());
    assert!(validate_template("%what:ever%").is_err());
    let ctx = SubfolderContext {
      now: chrono::Local::now(),
    };
    assert_eq!(render_template("%whatever%", &ctx), "%whatever%");
  }
}
