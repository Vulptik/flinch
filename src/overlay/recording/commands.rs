// Commands for recording overlay

/// Result returned from area recording selection overlay.
///
/// `x` and `y` are in Windows virtual screen coordinates and can be negative
/// (e.g., a monitor to the left of the primary monitor has negative x values).
#[derive(Debug, Clone)]
pub struct AreaRecordingResult {
  pub x: i32,
  pub y: i32,
  pub width: u32,
  pub height: u32,
  pub screen_width: u32,
  pub screen_height: u32,
  /// Exe stem of the source process under the selection center (e.g. "Code"). None if unknown.
  pub source_app: Option<String>,
}
