fn main() {
  if let Err(error) = flinch_video_lab::run(std::env::args_os().skip(1).collect()) {
    flinch_video_lab::report_error(&error);
    std::process::exit(1);
  }
}
