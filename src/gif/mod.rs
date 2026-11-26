// GIF recording module
//
// A GIF is recorded as a temporary MP4 by the native engine, then encoded natively in two passes:
// one palette for the whole recording, then Sierra-2-4A dithering and LZW compression.

pub mod session;

pub use session::{
  GIF_TEMP_MARKER, build_gif_output_path, build_gif_video_config, build_intermediate_output_path,
};
