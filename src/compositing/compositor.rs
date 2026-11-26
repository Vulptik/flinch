//! Multi-monitor compositor: computes slice geometry and blits captured frames
//! into a single output buffer.

use anyhow::{Result, bail};
use tracing::{debug, info, warn};

use crate::monitors::MonitorInfo;

use super::backend::{CaptureBackend, FrameAcquireResult};
use super::gdi_fallback::GdiMonitorBackend;

#[cfg(test)]
mod regression_tests;

/// Number of consecutive per-frame errors before escalating a slice's backend
/// from DXGI to GDI. At 30 fps this is ~1 second of sustained failure.
const ESCALATION_THRESHOLD: u32 = 30;

/// After reaching the terminal backend (GDI), how many more consecutive errors
/// before we give up and freeze the last good frame rather than flooding the
/// output with black fills.
const HOLD_LAST_FRAME_THRESHOLD: u32 = 30;

// ---------------------------------------------------------------------------
// Public configuration / geometry types
// ---------------------------------------------------------------------------

/// Selection rectangle in logical (virtual desktop) coordinates.
#[derive(Debug, Clone)]
pub struct CompositorConfig {
  pub selection_x: i32,
  pub selection_y: i32,
  pub selection_width: u32,
  pub selection_height: u32,
  pub show_cursor: bool,
}

/// A rectangle in physical pixel space (all coordinates are non-negative).
#[derive(Debug, Clone)]
pub struct CropRect {
  pub x: u32,
  pub y: u32,
  pub width: u32,
  pub height: u32,
}

/// Describes how one monitor's pixels map into the compositor output buffer.
#[derive(Debug, Clone)]
pub struct SliceGeometry {
  /// The source monitor.
  pub monitor: MonitorInfo,
  /// The sub-rectangle within the monitor's physical pixel grid to capture.
  pub src_rect: CropRect,
  /// Top-left corner in the output buffer where this slice is placed.
  pub dst_offset: (u32, u32),
  /// Optional scale factor applied when blitting (None = 1:1).
  pub scale_factor: Option<f64>,
  /// Expected physical dimensions of the monitor at capture start.
  /// Used to detect mid-recording resolution changes.
  pub monitor_width: u32,
  pub monitor_height: u32,
}

/// A live capture slice: geometry paired with its backend.
pub struct MonitorSlice {
  pub geometry: SliceGeometry,
  pub backend: Box<dyn CaptureBackend>,
  /// Consecutive per-frame errors from the current backend.
  /// Reset to 0 on a successful frame or after a backend escalation.
  consecutive_errors: u32,
  /// When true the backend has failed terminally. The output buffer retains
  /// the last good frame for this slice and `acquire_frame` is not called
  /// again. This prevents log flooding on permanently-disconnected monitors.
  hold_last_frame: bool,
  /// True when the slice is already using the GDI fallback backend.
  /// Prevents infinite re-escalation if GDI itself starts failing.
  is_gdi_backend: bool,
}

impl MonitorSlice {
  /// Create a new slice with the given geometry and capture backend.
  ///
  /// Set `is_gdi_backend` to `true` when the backend is a [`GdiMonitorBackend`]
  /// or [`GdiDesktopBackend`] so that the escalation path knows not to
  /// attempt a second DXGI → GDI switch.
  pub fn new(
    geometry: SliceGeometry,
    backend: Box<dyn CaptureBackend>,
    is_gdi_backend: bool,
  ) -> Self {
    Self {
      geometry,
      backend,
      consecutive_errors: 0,
      hold_last_frame: false,
      is_gdi_backend,
    }
  }
}

/// Output of a single `capture_frame` call.
pub struct CaptureResult<'a> {
  /// Reference into the compositor's output buffer.
  pub data: &'a [u8],
  /// `true` if at least one slice produced a new frame this call.
  pub changed: bool,
  /// `true` when `changed` is set solely because a backend error triggered
  /// `fill_black_region`. Callers must NOT save this frame to the CFR duplicate
  /// buffer — doing so would make all subsequent static-desktop duplicates black.
  pub had_error: bool,
}

// ---------------------------------------------------------------------------
// MultiMonitorCompositor
// ---------------------------------------------------------------------------

pub struct MultiMonitorCompositor {
  slices: Vec<MonitorSlice>,
  output: Vec<u8>,
  output_width: u32,
  output_height: u32,
}

impl MultiMonitorCompositor {
  // -----------------------------------------------------------------------
  // Geometry computation (stateless helper, public for unit testing)
  // -----------------------------------------------------------------------

  /// Compute the `SliceGeometry` list for the given monitors and selection.
  ///
  /// Returns `(geometries, output_width, output_height)` where the output
  /// dimensions describe the minimum rectangle that contains all slices.
  pub fn compute_slices(
    monitors: &[MonitorInfo],
    config: &CompositorConfig,
  ) -> Result<(Vec<SliceGeometry>, u32, u32)> {
    if monitors.is_empty() {
      bail!("No monitors provided");
    }

    // Reference DPI: taken from the primary monitor, or the first one.
    let reference_dpi = monitors
      .iter()
      .find(|m| m.is_primary)
      .unwrap_or(&monitors[0]) // safe: empty check above
      .dpi_scale;

    let sel_x = config.selection_x;
    let sel_y = config.selection_y;
    let sel_right = sel_x.saturating_add(config.selection_width as i32);
    let sel_bottom = sel_y.saturating_add(config.selection_height as i32);

    let mut slices: Vec<SliceGeometry> = Vec::new();

    for monitor in monitors {
      // Logical bounds of this monitor.
      let mon_lx = monitor.logical_x;
      let mon_ly = monitor.logical_y;
      let mon_lr = mon_lx.saturating_add(monitor.logical_width as i32);
      let mon_lb = mon_ly.saturating_add(monitor.logical_height as i32);

      // Logical intersection of selection with this monitor.
      let ix = sel_x.max(mon_lx);
      let iy = sel_y.max(mon_ly);
      let ir = sel_right.min(mon_lr);
      let ib = sel_bottom.min(mon_lb);

      if ir <= ix || ib <= iy {
        // No overlap with this monitor.
        continue;
      }

      // Intersection dimensions in logical pixels.
      let logical_w = (ir - ix) as u32;
      let logical_h = (ib - iy) as u32;

      // Offset of intersection within the monitor's logical rect.
      let local_lx = (ix - mon_lx) as u32;
      let local_ly = (iy - mon_ly) as u32;

      // Convert to physical pixels using this monitor's DPI scale.
      let scale = monitor.dpi_scale;
      let phys_x = (local_lx as f64 * scale).round() as u32;
      let phys_y = (local_ly as f64 * scale).round() as u32;
      let phys_w = (logical_w as f64 * scale).round() as u32;
      let phys_h = (logical_h as f64 * scale).round() as u32;

      // Clamp to actual monitor physical dimensions to be safe.
      let phys_x = phys_x.min(monitor.width.saturating_sub(1));
      let phys_y = phys_y.min(monitor.height.saturating_sub(1));
      let phys_w = phys_w.min(monitor.width.saturating_sub(phys_x));
      let phys_h = phys_h.min(monitor.height.saturating_sub(phys_y));

      if phys_w == 0 || phys_h == 0 {
        debug!(
            monitor = %monitor.id,
            "Physical crop rect has zero area after clamping; skipping monitor"
        );
        continue;
      }

      // Destination offset in the output buffer (logical coordinates
      // relative to the selection origin, then scaled to reference DPI).
      let dst_lx = (ix - sel_x) as u32;
      let dst_ly = (iy - sel_y) as u32;
      let dst_x = (dst_lx as f64 * reference_dpi).round() as u32;
      let dst_y = (dst_ly as f64 * reference_dpi).round() as u32;

      // Scale factor: how much we need to scale the captured pixels so
      // that they match the reference DPI output grid.
      let scale_factor = if (scale - reference_dpi).abs() < 1e-9 {
        None // same DPI — no scaling needed
      } else {
        Some(reference_dpi / scale)
      };

      slices.push(SliceGeometry {
        monitor: monitor.clone(),
        src_rect: CropRect {
          x: phys_x,
          y: phys_y,
          width: phys_w,
          height: phys_h,
        },
        dst_offset: (dst_x, dst_y),
        scale_factor,
        monitor_width: monitor.width,
        monitor_height: monitor.height,
      });
    }

    if slices.is_empty() {
      bail!("Selection does not overlap any monitor");
    }

    // Sort left-to-right, top-to-bottom by destination offset.
    slices.sort_by(|a, b| {
      a.dst_offset
        .1
        .cmp(&b.dst_offset.1)
        .then(a.dst_offset.0.cmp(&b.dst_offset.0))
    });

    // Output dimensions: bounding box of all slices in the output buffer.
    let out_w = slices
      .iter()
      .map(|s| {
        let w = s
          .scale_factor
          .map(|f| (s.src_rect.width as f64 * f).round() as u32)
          .unwrap_or(s.src_rect.width);
        s.dst_offset.0 + w
      })
      .max()
      .unwrap_or(0); // safe: slices is non-empty

    let out_h = slices
      .iter()
      .map(|s| {
        let h = s
          .scale_factor
          .map(|f| (s.src_rect.height as f64 * f).round() as u32)
          .unwrap_or(s.src_rect.height);
        s.dst_offset.1 + h
      })
      .max()
      .unwrap_or(0);

    Ok((slices, out_w, out_h))
  }

  // -----------------------------------------------------------------------
  // Construction
  // -----------------------------------------------------------------------

  /// Create a compositor from pre-built slices and a known output size.
  pub fn new(slices: Vec<MonitorSlice>, output_width: u32, output_height: u32) -> Self {
    let buf_size = (output_width as usize) * (output_height as usize) * 4;
    Self {
      slices,
      output: vec![0u8; buf_size],
      output_width,
      output_height,
    }
  }

  // -----------------------------------------------------------------------
  // Frame capture
  // -----------------------------------------------------------------------

  /// Capture a composite frame from all slices.
  ///
  /// On success, returns a reference to the output buffer and a flag
  /// indicating whether any slice produced new data.
  pub fn capture_frame(&mut self) -> Result<CaptureResult<'_>> {
    let mut changed = false;
    let mut had_error = false;

    for i in 0..self.slices.len() {
      // Slices in hold-last-frame mode have a permanently-failed backend.
      // Their region in the output buffer already holds the last good frame;
      // skip capture entirely so we neither log-flood nor overwrite with black.
      if self.slices[i].hold_last_frame {
        continue;
      }

      // Extract geometry into locals first to avoid simultaneous borrows.
      let dst_x = self.slices[i].geometry.dst_offset.0;
      let dst_y = self.slices[i].geometry.dst_offset.1;
      let src_w = self.slices[i].geometry.src_rect.width;
      let src_h = self.slices[i].geometry.src_rect.height;
      let src_x = self.slices[i].geometry.src_rect.x;
      let src_y = self.slices[i].geometry.src_rect.y;
      let scale_factor = self.slices[i].geometry.scale_factor;
      let mon_w = self.slices[i].geometry.monitor_width;
      let mon_h = self.slices[i].geometry.monitor_height;

      let frame = self.slices[i].backend.acquire_frame();

      match frame {
        Ok(FrameAcquireResult::NewFrame {
          data,
          width,
          height,
          row_pitch,
        }) => {
          changed = true;

          // Detect monitor resolution change mid-recording.
          // Compare returned frame dimensions against expected monitor dimensions.
          if width != mon_w || height != mon_h {
            warn!(
              slice_index = i,
              old_w = mon_w,
              old_h = mon_h,
              new_w = width,
              new_h = height,
              "Monitor resolution changed mid-recording; re-cropping"
            );
            // Clamp src_rect to new monitor bounds using locals computed above.
            let new_src_w = src_w.min(width);
            let new_src_h = src_h.min(height);
            let new_src_x = src_x.min(width.saturating_sub(new_src_w));
            let new_src_y = src_y.min(height.saturating_sub(new_src_h));

            // Blit before mutating self.slices to avoid borrow conflicts.
            blit_slice(
              &mut self.output,
              self.output_width,
              self.output_height,
              data,
              width,
              height,
              row_pitch,
              new_src_x,
              new_src_y,
              new_src_w,
              new_src_h,
              dst_x,
              dst_y,
              scale_factor,
            );

            // data borrow ends here — now safe to mutate self.slices[i].
            self.slices[i].geometry.monitor_width = width;
            self.slices[i].geometry.monitor_height = height;
            self.slices[i].geometry.src_rect.x = new_src_x;
            self.slices[i].geometry.src_rect.y = new_src_y;
            self.slices[i].geometry.src_rect.width = new_src_w;
            self.slices[i].geometry.src_rect.height = new_src_h;
            self.slices[i].consecutive_errors = 0;
            continue;
          }

          blit_slice(
            &mut self.output,
            self.output_width,
            self.output_height,
            data,
            width,
            height,
            row_pitch,
            src_x,
            src_y,
            src_w,
            src_h,
            dst_x,
            dst_y,
            scale_factor,
          );
          // data borrow ends here — now safe to mutate self.slices[i].
          self.slices[i].consecutive_errors = 0;
        },
        Ok(FrameAcquireResult::NoChange) => {
          // Output buffer already holds the previous frame — nothing to do.
          self.slices[i].consecutive_errors = 0;
        },
        Err(e) => {
          self.slices[i].consecutive_errors += 1;
          let errors = self.slices[i].consecutive_errors;

          if errors >= ESCALATION_THRESHOLD {
            // Try escalating from DXGI to GDI for this slice.
            if self.try_escalate_to_gdi(i) {
              self.slices[i].consecutive_errors = 0;
              // Fill black once during the transition frame.
              self.fill_black_region(i);
              changed = true;
              had_error = true;
            } else if errors >= ESCALATION_THRESHOLD + HOLD_LAST_FRAME_THRESHOLD {
              // GDI is also failing (e.g. monitor fully disconnected).
              // Freeze the last frame and stop trying.
              warn!(
                  slice_index = i,
                  monitor = %self.slices[i].geometry.monitor.name,
                  "Monitor unrecoverable after {} errors; freezing last frame",
                  errors,
              );
              self.slices[i].hold_last_frame = true;
              // No fill, no changed — output buffer keeps the last good pixels.
            } else {
              // GDI path but still failing: fill black, keep trying.
              warn!(
                  slice_index = i,
                  error = %e,
                  consecutive_errors = errors,
                  "Slice backend error (GDI); filling black"
              );
              self.fill_black_region(i);
              changed = true;
              had_error = true;
            }
          } else {
            // Below escalation threshold — fill black and keep trying.
            warn!(
                slice_index = i,
                error = %e,
                consecutive_errors = errors,
                "Failed to acquire frame from slice backend; filling black"
            );
            self.fill_black_region(i);
            changed = true;
            had_error = true;
          }
        },
      }
    }

    Ok(CaptureResult {
      data: &self.output,
      changed,
      had_error,
    })
  }

  /// Attempt to replace the backend for slice `i` with a GDI fallback.
  ///
  /// Returns `true` if the escalation happened (i.e. the slice was not
  /// already on GDI), `false` if already on GDI or if monitor dimensions
  /// are zero.
  fn try_escalate_to_gdi(&mut self, i: usize) -> bool {
    if self.slices[i].is_gdi_backend {
      // Already on the terminal backend — cannot escalate further.
      return false;
    }
    let monitor = &self.slices[i].geometry.monitor;
    if monitor.logical_width == 0 || monitor.logical_height == 0 {
      return false;
    }
    // Construct GDI backend before mutably borrowing self.slices.
    let gdi: Box<dyn CaptureBackend> = Box::new(GdiMonitorBackend::new(monitor));
    let old = std::mem::replace(&mut self.slices[i].backend, gdi);
    drop(old);
    self.slices[i].is_gdi_backend = true;
    info!(
        slice_index = i,
        monitor = %self.slices[i].geometry.monitor.name,
        "Escalated slice backend from DXGI to GDI"
    );
    true
  }

  // -----------------------------------------------------------------------
  // Helpers
  // -----------------------------------------------------------------------

  /// Fill the output region corresponding to `slice_index` with black pixels.
  pub fn fill_black_region(&mut self, slice_index: usize) {
    if slice_index >= self.slices.len() {
      warn!(slice_index, "fill_black_region: index out of bounds");
      return;
    }

    let (dst_x, dst_y) = self.slices[slice_index].geometry.dst_offset;
    let src_w = self.slices[slice_index].geometry.src_rect.width;
    let src_h = self.slices[slice_index].geometry.src_rect.height;
    let scale_factor = self.slices[slice_index].geometry.scale_factor;

    let out_w = scale_factor
      .map(|f| (src_w as f64 * f).round() as u32)
      .unwrap_or(src_w);
    let out_h = scale_factor
      .map(|f| (src_h as f64 * f).round() as u32)
      .unwrap_or(src_h);
    let out_w = out_w.min(self.output_width.saturating_sub(dst_x));
    let out_h = out_h.min(self.output_height.saturating_sub(dst_y));

    let stride = self.output_width as usize * 4;

    for row in 0..out_h {
      let y = dst_y + row;
      if y >= self.output_height {
        break;
      }
      let row_start = y as usize * stride + dst_x as usize * 4;
      let row_end = (row_start + out_w as usize * 4).min(self.output.len());
      if row_start >= self.output.len() {
        break;
      }
      self.output[row_start..row_end].fill(0);
    }
  }

  /// Raw reference to the output pixel buffer (BGRA, row-major).
  pub fn output(&self) -> &[u8] {
    &self.output
  }

  /// Output dimensions as `(width, height)`.
  pub fn dimensions(&self) -> (u32, u32) {
    (self.output_width, self.output_height)
  }

  /// Number of monitor slices in this compositor.
  pub fn slice_count(&self) -> usize {
    self.slices.len()
  }
}

// ---------------------------------------------------------------------------
// Blit helper (free function to avoid borrowing self during frame acquisition)
// ---------------------------------------------------------------------------

/// Copy (or scale) a captured frame crop into the output buffer.
#[allow(clippy::too_many_arguments)]
fn blit_slice(
  output: &mut [u8],
  out_w: u32,
  out_h: u32,
  src_data: &[u8],
  src_full_width: u32,
  src_full_height: u32,
  src_row_pitch: u32,
  crop_x: u32,
  crop_y: u32,
  crop_w: u32,
  crop_h: u32,
  dst_x: u32,
  dst_y: u32,
  scale_factor: Option<f64>,
) {
  let _ = (src_full_width, src_full_height); // validated by the backend

  match scale_factor {
    None => {
      // 1:1 copy — row-by-row memcpy.
      let crop_w = crop_w
        .min(src_full_width.saturating_sub(crop_x))
        .min(out_w.saturating_sub(dst_x));
      let crop_h = crop_h
        .min(src_full_height.saturating_sub(crop_y))
        .min(out_h.saturating_sub(dst_y));
      let out_stride = out_w as usize * 4;
      let src_pitch = src_row_pitch as usize;
      for row in 0..crop_h {
        let dst_row = dst_y + row;
        if dst_row >= out_h {
          break;
        }
        let src_row_index = crop_y + row;
        if src_row_index >= src_full_height {
          break;
        }
        let src_start = src_row_index as usize * src_pitch + crop_x as usize * 4;
        let src_end = src_start + crop_w as usize * 4;
        if src_end > src_data.len() {
          break;
        }
        let dst_start = dst_row as usize * out_stride + dst_x as usize * 4;
        let dst_end = (dst_start + crop_w as usize * 4).min(output.len());
        if dst_start >= output.len() {
          break;
        }
        let copy_len = (dst_end - dst_start).min(src_end - src_start);
        output[dst_start..dst_start + copy_len]
          .copy_from_slice(&src_data[src_start..src_start + copy_len]);
      }
    },
    Some(factor) => {
      // Scaled copy.  Use nearest-neighbour for exact integer ratios,
      // bilinear for fractional ratios.
      let is_integer = (factor - factor.round()).abs() < 1e-9 && factor > 0.0;
      if is_integer {
        blit_nearest(
          output,
          out_w,
          out_h,
          src_data,
          src_full_width,
          src_full_height,
          src_row_pitch,
          crop_x,
          crop_y,
          crop_w,
          crop_h,
          dst_x,
          dst_y,
          factor,
        );
      } else {
        blit_bilinear(
          output,
          out_w,
          out_h,
          src_data,
          src_full_width,
          src_full_height,
          src_row_pitch,
          crop_x,
          crop_y,
          crop_w,
          crop_h,
          dst_x,
          dst_y,
          factor,
        );
      }
    },
  }
}

/// Nearest-neighbour scale blit.
#[allow(clippy::too_many_arguments)]
fn blit_nearest(
  output: &mut [u8],
  out_w: u32,
  out_h: u32,
  src: &[u8],
  src_full_width: u32,
  src_full_height: u32,
  src_pitch: u32,
  crop_x: u32,
  crop_y: u32,
  src_w: u32,
  src_h: u32,
  dst_x: u32,
  dst_y: u32,
  factor: f64,
) {
  let dest_w = (src_w as f64 * factor).round() as u32;
  let dest_h = (src_h as f64 * factor).round() as u32;
  let out_stride = out_w as usize * 4;
  let s_pitch = src_pitch as usize;

  for dy in 0..dest_h {
    let oy = dst_y + dy;
    if oy >= out_h {
      break;
    }
    let sy = (crop_y + (dy as f64 / factor) as u32).min(src_full_height.saturating_sub(1));
    let src_row_start = sy as usize * s_pitch;

    for dx in 0..dest_w {
      let ox = dst_x + dx;
      if ox >= out_w {
        break;
      }
      let sx = (crop_x + (dx as f64 / factor) as u32).min(src_full_width.saturating_sub(1));
      let src_px = src_row_start + sx as usize * 4;
      let dst_px = oy as usize * out_stride + ox as usize * 4;
      if src_px + 4 > src.len() || dst_px + 4 > output.len() {
        continue;
      }
      output[dst_px..dst_px + 4].copy_from_slice(&src[src_px..src_px + 4]);
    }
  }
}

/// Bilinear scale blit.
#[allow(clippy::too_many_arguments)]
fn blit_bilinear(
  output: &mut [u8],
  out_w: u32,
  out_h: u32,
  src: &[u8],
  src_full_width: u32,
  src_full_height: u32,
  src_pitch: u32,
  crop_x: u32,
  crop_y: u32,
  src_w: u32,
  src_h: u32,
  dst_x: u32,
  dst_y: u32,
  factor: f64,
) {
  let dest_w = (src_w as f64 * factor).round() as u32;
  let dest_h = (src_h as f64 * factor).round() as u32;
  let out_stride = out_w as usize * 4;
  let s_pitch = src_pitch as usize;

  for dy in 0..dest_h {
    let oy = dst_y + dy;
    if oy >= out_h {
      break;
    }
    let sy_f = crop_y as f64 + (dy as f64 / factor);
    let sy0 = (sy_f.floor() as u32).min(src_full_height.saturating_sub(1));
    let sy1 = (sy0 + 1).min(src_full_height.saturating_sub(1));
    let ty = sy_f - sy0 as f64;

    for dx in 0..dest_w {
      let ox = dst_x + dx;
      if ox >= out_w {
        break;
      }
      let sx_f = crop_x as f64 + (dx as f64 / factor);
      let sx0 = (sx_f.floor() as u32).min(src_full_width.saturating_sub(1));
      let sx1 = (sx0 + 1).min(src_full_width.saturating_sub(1));
      let tx = sx_f - sx0 as f64;

      let p00 = src_pixel(src, s_pitch, sx0, sy0);
      let p10 = src_pixel(src, s_pitch, sx1, sy0);
      let p01 = src_pixel(src, s_pitch, sx0, sy1);
      let p11 = src_pixel(src, s_pitch, sx1, sy1);

      let mut blended = [0u8; 4];
      for c in 0..4 {
        let top = lerp(p00[c] as f64, p10[c] as f64, tx);
        let bot = lerp(p01[c] as f64, p11[c] as f64, tx);
        blended[c] = lerp(top, bot, ty).round().clamp(0.0, 255.0) as u8;
      }

      let dst_px = oy as usize * out_stride + ox as usize * 4;
      if dst_px + 4 > output.len() {
        continue;
      }
      output[dst_px..dst_px + 4].copy_from_slice(&blended);
    }
  }
}

#[inline]
fn src_pixel(src: &[u8], pitch: usize, x: u32, y: u32) -> [u8; 4] {
  let offset = y as usize * pitch + x as usize * 4;
  if offset + 4 > src.len() {
    return [0u8; 4];
  }
  [
    src[offset],
    src[offset + 1],
    src[offset + 2],
    src[offset + 3],
  ]
}

#[inline]
fn lerp(a: f64, b: f64, t: f64) -> f64 {
  a + (b - a) * t
}

#[cfg(test)]
mod tests {
  use super::blit_slice;

  #[test]
  fn test_blit_slice_respects_source_crop_offset() {
    let mut output = vec![0u8; 2 * 2 * 4];
    let src = vec![
      1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255, 7, 0, 0,
      255, 8, 0, 0, 255, 9, 0, 0, 255, 10, 0, 0, 255, 11, 0, 0, 255, 12, 0, 0, 255,
    ];

    blit_slice(&mut output, 2, 2, &src, 4, 3, 4 * 4, 1, 1, 2, 2, 0, 0, None);

    assert_eq!(
      output,
      vec![6, 0, 0, 255, 7, 0, 0, 255, 10, 0, 0, 255, 11, 0, 0, 255,]
    );
  }
}
