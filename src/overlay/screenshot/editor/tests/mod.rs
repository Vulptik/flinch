use super::layout::*;
use super::objects::{constrained_endpoint, shortcut_tool};
use super::*;
use crate::overlay::selection::SelectionInteraction;
use crate::overlay::selection_core::SelectionControllerState;
use crate::screenshot::annotations::{Label, MAX_POINTS, text_data};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{HDC, HFONT};

mod benchmark;
mod cursors;
mod geometry;
mod previews;
mod render;
mod toolbar;
mod transactions;

fn crop_tuple(controller: &SelectionControllerState) -> (f64, f64, f64, f64) {
  let rect = AnnotationEditor::confirmed(controller).expect("idle crop");
  (rect.x, rect.y, rect.w, rect.h)
}

#[test]
#[ignore = "explicit Windows rendering benchmark"]
fn rendering_performance_baseline() {
  benchmark::rendering_performance_baseline();
}

#[test]
#[ignore = "explicit offscreen Store image export"]
fn render_store_listing_images() {
  previews::render_store_listing_images();
}

#[test]
#[ignore = "explicit offscreen drawing toolbar previews"]
fn render_toolbar_previews() {
  previews::render_toolbar_previews();
}

#[test]
#[ignore = "explicit offscreen animated chrome preview"]
fn render_motion_preview() {
  previews::render_motion_preview();
}
