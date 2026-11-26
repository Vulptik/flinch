# Contributing to Flinch

Thanks for helping. Flinch is intentionally small and fast, so please open an issue to discuss a
new feature before writing it. Bug reports and fixes are always welcome.

Development happens in the open on `main`, so open pull requests against `main`. Releases are tags
such as `v1.3.0`. Contributions are licensed under the same terms as the code they change.

AI-assisted contributions are welcome. Mention it in the pull request if a tool wrote a large part
of the change, make sure the change works and its tests pass, and be ready to answer questions
about it.

## Where to start

Flinch runs on Windows for now. Ports to other platforms are welcome too; open an issue first so
we can talk through the approach. Run `cargo xtask doctor` to check your setup, and read the
[architecture overview](docs/architecture.md) first. Each row names where the code lives and the
nearest quiet check; broaden validation when a change crosses boundaries.

| Change | Code | Nearest quiet check |
| --- | --- | --- |
| Settings behavior or controls | `src/settings/`, `crates/settings/` | `cargo xtask test:settings` |
| Screenshot selection or annotations | `src/overlay/screenshot/`, `src/overlay/selection/` | `cargo test --no-default-features --features native-settings --lib overlay::screenshot -- --test-threads=1` |
| Recording lifecycle, timing or encoding | `src/video/controller/` | The nearest filtered test, then `cargo xtask test:recording` |
| Native video engine or lab | `crates/video-engine/`, `crates/video-lab/` | `cargo xtask test:video` |
| Tray, hotkeys, activation or native services | `src/app/`, `src/commands/` | `cargo xtask test:run` |
| Defaults, catalogs or packaging inputs | `resources/`, `messages/`, `crates/xtask/src/checks.rs` | `cargo xtask check:contracts` |
| Direct-download installer | `crates/installer/`, `windows/installer/` | `cargo xtask test:installer` |
| Docs | `README.md`, `CONTRIBUTING.md`, `docs/` | `cargo xtask check:docs` |
| Development tooling | `crates/xtask/`, `scripts/` | `cargo xtask test:tooling` |

`cargo xtask test:run --list` previews what a test mode runs before you start it.

## A few rules

- **Quiet checks by default.** Commands ending in `:desktop` open windows, capture the screen or
  register hotkeys; run them only when you mean to.
- **Bug fixes come with a test** that fails before the fix. Coverage must not drop below
  `scripts/coverage-policy.json` (`cargo xtask test:coverage`).
- **No panics:** production code doesn't use `unwrap()`, `expect()` or `panic!()`. Handle errors
  and timeouts explicitly.
- **User-facing text** goes through the `messages/` catalogs, in every supported language.

## License

The app is licensed under [GPL-3.0-only](LICENSE) and the installer under the MIT License; see
[NOTICE.md](NOTICE.md). The Flinch name and logo are covered by the
[trademark policy](TRADEMARKS.md).
