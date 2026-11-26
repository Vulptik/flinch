# Flinch Windows text rendering patch

Source: `i-slint-renderer-skia` 1.18.1 from crates.io, Slint commit
`372cf0ee5577c3dfec309a45e7b778ba4e81b734`, `internal/renderers/skia`.
Upstream licenses remain in `LICENSES/`. The normalized `Cargo.toml` is the build
input; `Cargo.toml.orig` retains upstream provenance.

Upstream issue: https://github.com/slint-ui/slint/issues/5748

The workspace Cargo patch applies to both settings and installer Slint UI.
Windows glyphs request LCD antialiasing when Windows enables ClearType. Opaque
software presentation and snapshot surfaces receive Windows' RGB/BGR pixel
geometry. Standard smoothing and disabled smoothing remain distinct. Transparent
intermediate surfaces keep unknown geometry so Skia can use safe grayscale
rendering there. Other operating systems retain upstream behavior.

Windows snapshots render into native BGRA (required for Skia LCD glyph masks),
then swap red and blue to preserve Slint's public RGBA snapshot contract.

`windows_text.rs` reads smoothing enablement, type and orientation once per
process. Restart Flinch after changing those Windows preferences. Query failures
are logged once and explicitly use grayscale antialiasing. Skia's existing gamma
and contrast defaults remain unchanged; this is not full DirectWrite rasterizer
parity or support for per-monitor subpixel-layout overrides.

Local changes: `windows_text.rs`, module registration and snapshot properties in
`lib.rs`, glyph edging in `itemrenderer.rs`, surface properties in
`software_surface.rs`, and the Windows UI API feature in `Cargo.toml`.

Run the quiet settings gate, which includes the patch regression tests. The
regressions cover disabled, grayscale, RGB/BGR and unknown preference values,
plus opaque sRGB glyph output at 100%, 125%, 150% and 200% scaling. Production
page snapshots and actual display acceptance remain separate checks.

On every Slint upgrade, replace this directory from the matching exact-version
crate, review and reapply only these changes, and rerun the regressions and
settings renders. Remove the Cargo patch when upstream implements equivalent
Windows smoothing behavior and the visual comparison passes.
