# MassFckinMailer

[![CI](https://github.com/ebesirik/MassFckinMailer/actions/workflows/ci.yml/badge.svg)](https://github.com/ebesirik/MassFckinMailer/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Cross-platform desktop mass mailer in Rust — gpui + gpui-component. Non-blocking,
smooth, lightweight. See [PLAN.md](PLAN.md) for the full design.

## Features

- **Providers**: generic SMTP, Mailgun, AWS SES, Gmail (OAuth), Outlook/Graph
  (OAuth). Credentials live in the OS keychain, never in project files.
- **Templates**: HTML editor with live preview and `{{field}}` / `##field##`
  placeholders (rendered with a strict minijinja environment — undefined fields
  are caught before sending). Auto-generated plain-text alternative.
- **Recipients**: streaming CSV + Excel/ODS import, automatic email-column
  detection, fuzzy field mapping, per-row validation, and a virtualized preview
  table that handles very large lists.
- **Send engine**: work queue + workers, per-account throttling, retry with
  backoff, a consecutive-failure circuit breaker, cooperative cancel, and live
  coalesced progress — all on a dedicated tokio runtime bridged to the UI over
  flume channels.
- **Outcome reports**: per-row CSV (status, error, provider message id) that can
  be re-loaded to **resume** only the failed/cancelled rows.
- **Projects**: human-readable TOML save/load, recent projects, dirty-state
  tracking with a discard prompt.

## Build & run

Requires stable Rust ≥ 1.85 (`rustup update`).

```
cargo run -p mmm-app          # launch the app
cargo test --workspace        # pure logic + engine bridge + providers
cargo clippy --workspace
```

## Packaging

The app icon lives at [`assets/icon.svg`](assets/icon.svg); the raster/ICO
versions are regenerated from it with:

```
cargo run -p mmm-app --example gen_icon   # writes assets/icon.{png,ico}
```

On Windows the icon is embedded in the `.exe` automatically at build time
(`crates/app/build.rs` via `winresource`). Installer builds use the
`[package.metadata.bundle]` metadata in `crates/app/Cargo.toml`:

| Target | Tool | Command |
|---|---|---|
| Windows `.msi` | [`cargo-wix`](https://github.com/volks73/cargo-wix) (needs the WiX Toolset) | `cargo wix -p mmm-app` |
| macOS `.app`/`.dmg` | [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle) | `cargo bundle -p mmm-app --release` |
| Linux `.deb` | `cargo-bundle` | `cargo bundle -p mmm-app --release` |
| Linux AppImage | [`cargo-appimage`](https://github.com/StratusFearMe21/cargo-appimage) | `cargo appimage` |

Each installer must be produced on (or cross-compiled for) its own platform.

## Continuous integration & release channels

GitHub Actions (`.github/workflows/`) drive builds across Linux, macOS, and
Windows:

- **CI** (`ci.yml`) — on pushes to `master`/`beta`/`dev` and all PRs: rustfmt,
  clippy, tests, and a release build on every platform.
- **Channels** (`channels.yml`) — pushing a channel branch publishes a *rolling*
  pre-release (one release per channel, updated in place):
  - `beta` → the **Beta** pre-release (tag `beta`)
  - `dev` → the **Nightly** pre-release (tag `nightly`)
- **Stable** (`release.yml`) — pushing a `v*` tag (e.g. `v0.1.0`) builds all
  platforms and attaches archives to a **draft** GitHub Release for review.
  `master` never auto-releases; stable ships from tags.

Every release (stable and channel) ships a portable archive per platform, and
**Windows additionally gets a one-click installer** (`…-setup.exe`) built with
[Inno Setup](https://jrsoftware.org/isinfo.php) from
[`installer/windows/massfckinmailer.iss`](installer/windows/massfckinmailer.iss).

### Versioning

The single source of truth is `[workspace.package] version` in the root
`Cargo.toml` (all crates inherit it). Pipelines derive the displayed version
from it and append a build number (the GitHub Actions run number):

- stable → the tag, e.g. `0.1.0`
- beta → `0.1.0-beta.<run>`
- nightly → `0.1.0-nightly.<yyyymmdd>.<run>`

That string is embedded at build time (`MFM_VERSION`, wired through
`crates/app/build.rs`) and shown in the app's sidebar. Local builds fall back to
the plain `Cargo.toml` version.

No repository secrets are required for any of the above. Code signing /
notarization in `release.yml` activates automatically once you add the
relevant secrets — the full walkthrough (getting a Developer ID certificate
from your Apple account, notarization credentials, Windows options) is in
[docs/SIGNING.md](docs/SIGNING.md). Signed macOS builds additionally ship a
notarized `.dmg`.

### Auto-update (OTA via GitHub)

The app updates itself over the air using **GitHub Releases as the only backend**
— no server to run. On startup it asks the public GitHub API for the newest
release **on its own channel** (inferred from `MFM_VERSION`: stable →
`/releases/latest`, beta/nightly → the rolling tag), compares with semver, and if
newer shows a **"Restart & update"** button in the sidebar — notify-and-click, it
never auto-applies. On click it downloads the matching platform asset, verifies
its SHA-256 when GitHub supplies a digest, then hands off: **Windows** runs the
installer silently (it closes, upgrades, and relaunches the app), while
**Linux/macOS** swap the running binary in place and relaunch. Offline / rate-
limited checks fail silently. Implementation: `crates/engine/src/update.rs`.

## OAuth setup (Gmail / Outlook)

These providers require you to register your own OAuth app (no secret is shipped):

- **Gmail** — Google Cloud: enable the Gmail API, create an OAuth client of type
  *Desktop app*, and paste its client ID + secret when adding the account.
- **Outlook** — Azure: register an app, add the delegated **Mail.Send**
  permission, and under *Mobile and desktop applications* add redirect URI
  `http://localhost` (no client secret needed — public client + PKCE).

"Connect & authorize" opens your browser; tokens are stored in the keychain and
refreshed automatically.

## Version pinning

`gpui` is pre-1.0 and breaks between versions. `gpui`, `gpui-component`, and
`gpui-component-assets` are pinned exactly in the workspace `Cargo.toml`
(gpui-component 0.5.1 ↔ gpui 0.2.2) — upgrade all together, deliberately.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
