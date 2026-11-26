//! Redact All Text: covers every text line the detector finds in the crop with a Blur drawing.
//!
//! Detection runs on its own thread from the frozen capture, never on the overlay thread. It
//! starts as soon as Blur becomes the tool or the pointer reaches the button, so a click
//! usually finds the lines ready. The result is posted back as `READY`; the overlay thread
//! alone touches the editor. A job belongs to one crop: a different crop starts a new job and
//! cancels the old one, as do Esc while waiting and closing the overlay.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use super::{AnnotationEditor, COLORS, DrawingLimit};
use crate::ocr::{OcrError, OcrLimits, TextBox};
use crate::overlay::screenshot::history;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Annotation, MAX_ANNOTATIONS, Tool};

/// Posted to the overlay window when a detection job ends; `WPARAM` carries the job id.
pub(crate) const READY: u32 = WM_APP + 39;
/// Extra cover around each detected line, in capture pixels.
const PAD: i32 = 2;
const DEADLINE: Duration = Duration::from_secs(30);

/// Left, top, right (exclusive) and bottom (exclusive), in capture pixels.
pub(super) type Bounds = [i32; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Failure {
  TooLarge,
  Failed,
}

type Outcome = Result<Vec<TextBox>, Failure>;

struct Job {
  id: u64,
  crop: Bounds,
  cancel: Arc<AtomicBool>,
  result: Arc<Mutex<Option<Outcome>>>,
}

#[derive(Default)]
pub(super) struct Redaction {
  job: Option<Job>,
  jobs: u64,
  /// The user asked for covers; they are added when the job ends.
  pending: bool,
}

impl Redaction {
  pub(super) fn busy(&self) -> bool {
    self.pending
  }

  pub(super) fn cancel(&mut self) {
    if let Some(job) = self.job.take() {
      job.cancel.store(true, Ordering::Release);
    }
    self.pending = false;
  }

  fn finished(&self) -> Option<Outcome> {
    let job = self.job.as_ref()?;
    let slot = match job.result.lock() {
      Ok(slot) => slot,
      Err(poisoned) => poisoned.into_inner(),
    };
    slot.clone()
  }
}

impl Drop for Redaction {
  fn drop(&mut self) {
    self.cancel();
  }
}

impl AnnotationEditor {
  /// The confirmed crop in whole capture pixels, inside the frozen capture.
  fn redaction_crop(&self, controller: &SelectionControllerState) -> Option<Bounds> {
    let rect = Self::confirmed(controller)?;
    let frozen = self.frozen.as_ref()?;
    let (width, height) = (frozen.width as f64, frozen.height as f64);
    let left = rect.x.round().clamp(0.0, width) as i32;
    let top = rect.y.round().clamp(0.0, height) as i32;
    let right = (rect.x + rect.w).round().clamp(0.0, width) as i32;
    let bottom = (rect.y + rect.h).round().clamp(0.0, height) as i32;
    (right > left && bottom > top).then_some([left, top, right, bottom])
  }

  /// Starts detecting the crop's text in the background, unless that crop's job already runs.
  pub(super) fn prefetch_redaction(&mut self, controller: &SelectionControllerState, hwnd: HWND) {
    let Some(crop) = self.redaction_crop(controller) else {
      return;
    };
    if self
      .redaction
      .job
      .as_ref()
      .is_some_and(|job| job.crop == crop)
    {
      return;
    }
    if let Some(old) = self.redaction.job.take() {
      old.cancel.store(true, Ordering::Release);
    }
    let Some(frozen) = &self.frozen else {
      return;
    };
    self.redaction.jobs = self.redaction.jobs.wrapping_add(1);
    let job = Job {
      id: self.redaction.jobs,
      crop,
      cancel: Arc::new(AtomicBool::new(false)),
      result: Arc::new(Mutex::new(None)),
    };
    let (pixels, frozen_width) = (Arc::clone(&frozen.rgba), frozen.width);
    let (id, cancel, result) = (job.id, Arc::clone(&job.cancel), Arc::clone(&job.result));
    let window = hwnd.0 as isize;
    let spawned = std::thread::Builder::new()
      .name("redact-text".into())
      .spawn(move || {
        let started = Instant::now();
        let limits = OcrLimits {
          cancel,
          deadline: started + DEADLINE,
        };
        let Some(outcome) = detect(&pixels, frozen_width, crop, &limits) else {
          return;
        };
        crate::commands::log_speed_snapshot("screenshot.redact.detect", started.elapsed());
        store(&result, outcome);
        // SAFETY: posting to a window that has since closed fails harmlessly; nothing is borrowed.
        if let Err(error) = unsafe {
          PostMessageW(
            Some(HWND(window as *mut _)),
            READY,
            WPARAM(id as usize),
            LPARAM(0),
          )
        } {
          tracing::debug!(%error, "Text redaction finished after its overlay closed");
        }
      });
    if let Err(error) = spawned {
      tracing::error!(%error, "Cannot start text redaction");
      store(&job.result, Err(Failure::Failed));
    }
    self.redaction.job = Some(job);
  }

  /// Redact All Text, from the toolbar button or Shift+B.
  pub(super) fn redact_all_text(&mut self, controller: &SelectionControllerState, hwnd: HWND) {
    self.clear_toolbar_error();
    self.prefetch_redaction(controller, hwnd);
    if self.redaction.job.is_none() {
      return;
    }
    self.redaction.pending = true;
    self.finish_redaction(controller);
  }

  /// A detection job ended; `id` tells stale jobs apart.
  pub fn redaction_ready(&mut self, controller: &SelectionControllerState, hwnd: HWND, id: u64) {
    let Some(job) = &self.redaction.job else {
      return;
    };
    if job.id != id {
      return;
    }
    if self.redaction_crop(controller) != Some(job.crop) {
      // The crop changed while detecting; a waiting request follows the new crop.
      let pending = self.redaction.pending;
      self.redaction.cancel();
      if pending {
        self.redact_all_text(controller, hwnd);
      }
    } else {
      self.finish_redaction(controller);
    }
    super::layout::invalidate(hwnd);
  }

  fn finish_redaction(&mut self, controller: &SelectionControllerState) {
    if !self.redaction.pending {
      return;
    }
    let Some(outcome) = self.redaction.finished() else {
      return;
    };
    self.redaction.pending = false;
    let Some(crop) = self.redaction.job.as_ref().map(|job| job.crop) else {
      return;
    };
    if Some(crop) != self.redaction_crop(controller) {
      return;
    }
    match outcome {
      Ok(lines) => self.cover_lines(&lines, crop),
      Err(Failure::TooLarge) => self.redact_notice = Some("annotation_error_redact_large"),
      Err(Failure::Failed) => self.redact_notice = Some("annotation_error_redact_failed"),
    }
  }

  /// Adds one Blur drawing per line, as one undo step.
  pub(super) fn cover_lines(&mut self, lines: &[TextBox], crop: Bounds) {
    if lines.is_empty() {
      self.redact_notice = Some("annotation_error_redact_none");
      return;
    }
    let capacity = MAX_ANNOTATIONS.saturating_sub(self.annotations.len());
    if capacity == 0 {
      self.limited = Some(DrawingLimit::Drawings);
      return;
    }
    let covers = plan(lines, crop, capacity);
    let Some(next) = self.next_id.checked_add(covers.len() as u64) else {
      self.limited = Some(DrawingLimit::Identifier);
      tracing::error!("Screenshot annotation identifier limit reached");
      return;
    };
    let start = self.annotations.len();
    let changes = covers
      .iter()
      .enumerate()
      .map(|(offset, [left, top, right, bottom])| history::Change {
        counter: None,
        index: start + offset,
        before: None,
        after: Some(Annotation {
          corner_radius: 0,
          rotation_millidegrees: 0,
          path_transform: crate::screenshot::path::IDENTITY,
          curve_controls: Vec::new(),
          pen_nodes: Vec::new(),
          label: None,
          id: self.next_id + offset as u64,
          tool: Tool::Blur,
          points: vec![(*left, *top), (*right, *bottom)],
          color: COLORS[self.color],
          width: self.width,
        }),
      })
      .collect();
    match self.history.commit_batch(&mut self.annotations, changes) {
      Ok(true) => {
        self.next_id = next;
        self.selected = None;
        self.group_selection.clear();
        self.document_changed();
        tracing::debug!(
          lines = lines.len(),
          covers = covers.len(),
          "Text redaction added"
        );
      },
      Ok(false) => {},
      Err(error) => {
        tracing::error!(%error, "Failed to add text redaction");
        self.report_render_failure();
      },
    }
  }
}

fn store(slot: &Mutex<Option<Outcome>>, outcome: Outcome) {
  match slot.lock() {
    Ok(mut slot) => *slot = Some(outcome),
    Err(poisoned) => *poisoned.into_inner() = Some(outcome),
  }
}

/// Copies the crop out of the frozen capture and finds its lines. `None` when cancelled.
fn detect(pixels: &[u8], frozen_width: u32, crop: Bounds, limits: &OcrLimits) -> Option<Outcome> {
  let [left, top, right, bottom] = crop;
  let (width, height) = ((right - left) as usize, (bottom - top) as usize);
  let stride = frozen_width as usize * 4;
  let mut rgba = Vec::new();
  if rgba.try_reserve_exact(width * height * 4).is_err() {
    tracing::error!(width, height, "Cannot allocate the text redaction image");
    return Some(Err(Failure::Failed));
  }
  for row in top as usize..bottom as usize {
    let start = row * stride + left as usize * 4;
    let Some(line) = pixels.get(start..start + width * 4) else {
      tracing::error!("Text redaction crop lies outside the frozen capture");
      return Some(Err(Failure::Failed));
    };
    rgba.extend_from_slice(line);
  }
  match crate::ocr::detect_lines(&rgba, width as u32, height as u32, limits) {
    Ok(lines) => Some(Ok(lines)),
    Err(OcrError::Cancelled) => None,
    Err(OcrError::TooLarge) => Some(Err(Failure::TooLarge)),
    Err(error) => {
      tracing::warn!(%error, "Text redaction failed");
      Some(Err(Failure::Failed))
    },
  }
}

/// Turns lines found in the crop into covers in capture pixels: padded, kept inside the crop,
/// overlaps joined, and neighbors merged into blocks until they fit `capacity` drawings.
pub(super) fn plan(lines: &[TextBox], crop: Bounds, capacity: usize) -> Vec<Bounds> {
  let [left, top, right, bottom] = crop;
  let mut covers: Vec<Bounds> = lines
    .iter()
    .map(|line| {
      [
        (left + line.left - PAD).max(left),
        (top + line.top - PAD).max(top),
        (left + line.right + PAD).min(right),
        (top + line.bottom + PAD).min(bottom),
      ]
    })
    .filter(|[l, t, r, b]| r > l && b > t)
    .collect();
  merge_near(&mut covers, 0);
  let mut gap = 4;
  while covers.len() > capacity.max(1) {
    merge_near(&mut covers, gap);
    if gap > (right - left).max(bottom - top) {
      // Everything is one block now; nothing stays uncovered.
      break;
    }
    gap = gap.saturating_mul(2);
  }
  covers.sort_by_key(|[l, t, ..]| (*t, *l));
  covers
}

/// Joins covers that overlap or lie within `gap` pixels of each other, until none do.
fn merge_near(covers: &mut Vec<Bounds>, gap: i32) {
  let near = |a: &Bounds, b: &Bounds| {
    let across = (b[0] - a[2]).max(a[0] - b[2]);
    let down = (b[1] - a[3]).max(a[1] - b[3]);
    across <= gap && down <= gap && (across < 0 || down < 0 || gap > 0)
  };
  let mut merged = true;
  while merged {
    merged = false;
    let mut index = 0;
    while index < covers.len() {
      let mut other = index + 1;
      while other < covers.len() {
        if near(&covers[index], &covers[other]) {
          let joined = covers.swap_remove(other);
          let cover = &mut covers[index];
          *cover = [
            cover[0].min(joined[0]),
            cover[1].min(joined[1]),
            cover[2].max(joined[2]),
            cover[3].max(joined[3]),
          ];
          merged = true;
        } else {
          other += 1;
        }
      }
      index += 1;
    }
  }
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
