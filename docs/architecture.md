# Architecture

Flinch is a Windows tray app written in Rust. Settings use Slint; capture, annotation and
recording overlays are native Win32 windows; video and GIFs are recorded by Flinch's own engine in
a separate worker process.

## Threads and processes

```text
Main thread: Slint settings, tray menu, global hotkeys, single-instance window
    -> typed services and bounded event-loop callbacks
    -> Tokio workers: configuration, recording control, platform operations
    -> engine worker process: capture, encoding, audio, MP4 and GIF writing
    -> one dedicated thread per overlay: selection, annotation, recording bar, notifications
```

- **The main thread owns all Slint UI.** Workers never hold Slint components; they submit bounded,
  checked callbacks to the event loop.
- **Each overlay owns its own thread**, window, message loop, graphics resources and interaction
  state. Overlays draw with GDI buffers and shared Direct2D/DirectWrite controls.
- **`app/context.rs` owns the typed services** (configuration, clipboard, recording,
  notifications, lifecycle). There is no dynamic registry.
- **The recorder runs out of process.** `video/controller/` drives the engine worker in
  `crates/video-engine`; a crashed or stuck worker is contained, and its partial recording and
  diagnostics are kept.
- **Configuration edits** merge into the full configuration, validate before side effects and
  persist before they take effect.

## Repository layout

| Path | What |
| --- | --- |
| `src/app/` | Startup, typed context, Tokio runtime, tray, hotkeys and Windows adapters |
| `src/settings/` | Native settings presenter and configuration services |
| `src/overlay/` | Screenshot selection and annotation, recording and GIF selection, notifications |
| `src/screenshot/`, `src/compositing/` | Capture, encoding and Windows capture backends |
| `src/video/`, `src/gif/`, `src/recording/` | Recording control and the bridge to the engine |
| `src/commands/` | Typed operations used by capture workflows and settings |
| `src/config/`, `src/shortcuts/`, `src/monitors/` | Configuration, hotkeys, monitor geometry and DPI |
| `crates/settings/` | Slint settings UI and its model |
| `crates/video-engine/` | Native recording engine: capture, GPU conversion, encoders, audio, MP4 and GIF |
| `crates/video-lab/` | Standalone lab for driving the engine offscreen or on the desktop |
| `crates/blur/` | Blur used by the annotation tools |
| `crates/installer/`, `windows/installer/` | Direct-download WiX Burn/MSI installer (MIT) |
| `crates/xtask/` | `cargo xtask` checks, tests and packaging |
| `messages/` | Shared translations for every supported language |
| `resources/`, `icons/` | Bundled defaults, OCR models, cursors and branding |
| `vendor/` | Patched third-party crates, each with its patch notes |
| `tests/` | Integration suites |

## Distribution

Every release ships two packages from the same source and version: a Microsoft Store MSIX
(`cargo xtask pack:msix`) and a direct-download installer (`cargo xtask pack:installer`). Store
builds update through the Store; installer builds can check the latest GitHub release for a newer
version.
