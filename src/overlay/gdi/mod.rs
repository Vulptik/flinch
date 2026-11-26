//! Win32 + GDI overlay rendering layer.
//! Replaces wgpu/winit with native Windows GDI for reliable rendering on all hardware.

pub mod bitmap;
pub mod drawing;
pub mod layered;
pub mod window;

#[cfg(test)]
mod selection_previews;
