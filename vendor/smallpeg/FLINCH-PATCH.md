# Flinch smallpeg patch

Source: `smallpeg` 0.17.0 from crates.io (https://github.com/sawapi/smallpeg),
MIT OR Apache-2.0; upstream licenses remain in `LICENSE-MIT` and `LICENSE-APACHE`.
The normalized `Cargo.toml` is the build input; `Cargo.toml.orig` retains upstream
provenance. `Cargo.lock` is upstream's; `test:smallpeg` builds the crate on its own
with `--locked`. `rustfmt.toml` keeps upstream's default formatting instead of Flinch's.

Flinch encodes JPG screenshots with smallpeg and its `parallel` feature (see
`src/screenshot/encoding/jpeg.rs`). Since 0.16.0 that feature runs every encode stage on
worker threads (contributed from Flinch as sawapi/smallpeg#22): color conversion and DCT,
symbol statistics, sparse AC views, the progressive scan search and output, and both
trellis passes. `Encoder::threads` sets the count (0 = available cores up to 8, at most 64;
one worker per 1024 blocks, so small images stay on the calling thread). Output is
byte-identical to the single-thread encoder for every input and thread count. The
experimental `jxl` feature is in the source but not enabled.

`src/` is the 0.17.0 crate plus one Flinch-only layer:

**`Encoder::encode_rgb_until`.** It checks a stop callback after the DCT, after the
trellis, after the baseline statistics, after the sparse views and before the scans are
written, and returns `None` once it fires; `encode_rgb` is that with a callback that never
fires. Flinch uses it to abandon a speculative JPG when a PNG screenshot stays under 2 MB.
Not offered upstream (too specific to Flinch). Touches `lib.rs`, `document.rs`,
`progressive.rs`, `optimize.rs` and `cmyk.rs`; `stop_tests` in `lib.rs` covers it.

`cargo xtask test:smallpeg` runs the crate's tests, including upstream's, with the
feature on; it is part of `check:quick`.

To upgrade: diff this `src/` against the crate of the version named above to get the stop
hook alone, replace this directory from the new exact-version crate, reapply that diff, and
rerun `test:smallpeg`. Check that every upstream fix survived the reapply (a hunk that also
touches upstream code can revert it). Remove the Cargo patch entirely if upstream ever gains
an equivalent stop hook.
