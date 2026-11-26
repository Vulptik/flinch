// Message-passing architecture for screenshot overlay render thread

use super::selection::{Modifiers, SelectionInteraction};

/// Messages sent from event thread to render thread
#[derive(Debug, Clone)]
pub enum RenderMessage {
  /// Update the selection state (single source of truth)
  UpdateSelection {
    interaction: SelectionInteraction,
    cursor: (f64, f64),
    modifiers: Modifiers,
  },
  /// Request an immediate redraw
  Redraw,
  /// Shutdown the render thread
  Shutdown,
}

impl RenderMessage {
  /// Helper to create an update message from current state
  pub fn update(
    interaction: SelectionInteraction,
    cursor: (f64, f64),
    modifiers: Modifiers,
  ) -> Self {
    Self::UpdateSelection {
      interaction,
      cursor,
      modifiers,
    }
  }
}
